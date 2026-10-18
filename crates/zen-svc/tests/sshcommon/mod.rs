//! The SSH engine's test harness: a small application on the engine, the
//! way a handler runs it, restoring the engine from its state at every
//! step; a TCP server for OpenSSH; and an in-process pump for the test
//! client.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zen_svc::ssh::{Config, Conn, Error, Event, HostKey, Options, Progress, PublicKey};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn host_key() -> HostKey {
    HostKey::from_seed(&[42; 32])
}

pub fn config(opts: Options) -> Config {
    Config::new(host_key(), opts).unwrap()
}

/// What a channel does.
#[derive(Clone, Debug)]
enum Mode {
    /// Waiting for its request.
    Idle,
    /// `echo <text>`: the text and a newline, then exit 0.
    Echo(Vec<u8>),
    /// `cat`: standard input to standard output until EOF.
    Cat,
    /// `big <n>`: n bytes (`i % 251`), then exit 0.
    Big { sent: u64, n: u64 },
    /// `exit <code>`: `bye` on standard error, then that status.
    Exit(u32),
    /// A line shell: `> <line>` for each line.
    Shell,
    /// The `test-sub` subsystem: `sub:` before each chunk.
    Sub,
    /// Done.
    Ended,
}

/// The application: authorizes keys, runs the channels' commands.
pub struct App {
    pub authorized: Vec<PublicKey>,
    pub users: Vec<String>,
    modes: HashMap<u32, Mode>,
    /// Questions answered (signed or not), for tests.
    pub asked: Vec<(String, bool)>,
}

impl App {
    pub fn new(authorized: Vec<PublicKey>) -> App {
        App {
            authorized,
            users: vec!["test".into()],
            modes: HashMap::new(),
            asked: Vec::new(),
        }
    }

    /// Handles a step's events, then lets every channel make progress.
    /// Whether anything happened.
    pub fn step(
        &mut self,
        c: &mut Conn,
        events: &[Event],
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        let mut moved = !events.is_empty();
        for e in events {
            match e {
                Event::Authorize(a) => {
                    self.asked.push((a.user.clone(), a.signed));
                    let ok = self.users.contains(&a.user) && self.authorized.contains(&a.key);
                    out.extend(c.authorize(ok)?);
                }
                Event::ChannelOpen { ch } => {
                    self.modes.insert(*ch, Mode::Idle);
                }
                Event::Exec { ch, command } => {
                    let cmd = String::from_utf8_lossy(command).into_owned();
                    let mode = match cmd.split_once(' ') {
                        Some(("echo", t)) => Mode::Echo(t.as_bytes().to_vec()),
                        Some(("big", n)) => Mode::Big {
                            sent: 0,
                            n: n.parse().unwrap(),
                        },
                        Some(("exit", n)) => Mode::Exit(n.parse().unwrap()),
                        _ if cmd == "cat" => Mode::Cat,
                        _ => Mode::Exit(127),
                    };
                    self.modes.insert(*ch, mode);
                }
                Event::Shell { ch } => {
                    self.modes.insert(*ch, Mode::Shell);
                }
                Event::Subsystem { ch, name } => {
                    let m = if name == "test-sub" {
                        Mode::Sub
                    } else {
                        Mode::Exit(127)
                    };
                    self.modes.insert(*ch, m);
                }
                Event::Close { ch } => {
                    self.modes.remove(ch);
                }
                Event::Data { .. } | Event::Eof { .. } => {}
            }
        }
        let chans = c.channels();
        for info in chans {
            let ch = info.ch;
            let Some(mode) = self.modes.get_mut(&ch) else {
                continue;
            };
            if info.closed {
                continue;
            }
            let end = |c: &mut Conn, code: u32, out: &mut Vec<u8>| -> Result<(), Error> {
                out.extend(c.exit_status(ch, code)?);
                out.extend(c.eof(ch)?);
                out.extend(c.close(ch)?);
                Ok(())
            };
            match mode {
                Mode::Idle | Mode::Ended => {}
                Mode::Echo(t) => {
                    let mut line = std::mem::take(t);
                    line.push(b'\n');
                    let (n, o) = c.write(ch, &line)?;
                    assert_eq!(n, line.len());
                    out.extend(o);
                    end(c, 0, out)?;
                    *mode = Mode::Ended;
                    moved = true;
                }
                Mode::Exit(code) => {
                    let code = *code;
                    out.extend(c.write_stderr(ch, b"bye\n")?.1);
                    end(c, code, out)?;
                    *mode = Mode::Ended;
                    moved = true;
                }
                Mode::Big { sent, n } => {
                    while *sent < *n {
                        let len = (*n - *sent).min(65_536) as usize;
                        let chunk: Vec<u8> =
                            (0..len as u64).map(|i| ((*sent + i) % 251) as u8).collect();
                        let (taken, o) = c.write(ch, &chunk)?;
                        out.extend(o);
                        *sent += taken as u64;
                        if taken == 0 {
                            break;
                        }
                        moved = true;
                    }
                    if *sent == *n {
                        end(c, 0, out)?;
                        *mode = Mode::Ended;
                        moved = true;
                    }
                }
                Mode::Cat | Mode::Sub => {
                    let sub = matches!(mode, Mode::Sub);
                    let data = c.channel_data(ch).to_vec();
                    if !data.is_empty() {
                        let (taken, o) = if sub {
                            let mut d = b"sub:".to_vec();
                            d.extend_from_slice(&data);
                            let (t, o) = c.write(ch, &d)?;
                            // All or nothing, for the test's framing.
                            assert!(t == 0 || t == d.len() || t >= 4, "partial subsystem write");
                            (t.saturating_sub(4), o)
                        } else {
                            c.write(ch, &data)?
                        };
                        out.extend(o);
                        if taken > 0 {
                            out.extend(c.consume(ch, taken)?);
                            moved = true;
                        }
                    }
                    if info.eof && c.channel_data(ch).is_empty() {
                        end(c, 0, out)?;
                        *mode = Mode::Ended;
                        moved = true;
                    }
                }
                Mode::Shell => {
                    let data = c.channel_data(ch).to_vec();
                    let mut used = 0;
                    while let Some(nl) = data[used..].iter().position(|&b| b == b'\n') {
                        let mut reply = b"> ".to_vec();
                        reply.extend_from_slice(&data[used..used + nl + 1]);
                        let (t, o) = c.write(ch, &reply)?;
                        out.extend(o);
                        if t < reply.len() {
                            panic!("the shell's output doesn't fit");
                        }
                        used += nl + 1;
                    }
                    if used > 0 {
                        out.extend(c.consume(ch, used)?);
                        moved = true;
                    }
                    if info.eof && c.channel_data(ch).is_empty() {
                        end(c, 0, out)?;
                        *mode = Mode::Ended;
                        moved = true;
                    }
                }
            }
        }
        Ok(moved)
    }
}

/// What a served connection did.
#[derive(Debug, Default)]
pub struct Stats {
    pub error: Option<Error>,
    pub kexes: u64,
    pub max_state: usize,
    pub steps: u64,
    pub ciphers: Option<(zen_svc::ssh::Cipher, zen_svc::ssh::Cipher)>,
    pub user: Option<String>,
    pub asked: Vec<(String, bool)>,
}

/// One connection, driven step by step with a restore before each.
pub struct Driver {
    pub config: Config,
    pub state: Vec<u8>,
    pub app: App,
    pub buf: Vec<u8>,
    pub eof: bool,
    pub stats: Stats,
    pub ended: bool,
    /// Show the engine this many more bytes per step (0: all of them),
    /// as if input arrived in small pieces.
    pub feed: usize,
    shown: usize,
}

impl Driver {
    pub fn new(config: Config, app: App) -> (Driver, Vec<u8>) {
        let (c, out) = Conn::new(&config, now());
        let state = c.state();
        let d = Driver {
            config,
            state,
            app,
            buf: Vec::new(),
            eof: false,
            stats: Stats::default(),
            ended: false,
            feed: 0,
            shown: 0,
        };
        (d, out)
    }

    /// One step: restore, input, the application, save. The output and
    /// whether anything moved.
    pub fn step(&mut self) -> (Vec<u8>, bool) {
        let mut c = Conn::restore(&self.config, &self.state).expect("the state restores");
        let n = if self.feed == 0 {
            self.buf.len()
        } else {
            self.buf.len().min(self.shown + self.feed)
        };
        let eof = self.eof && n == self.buf.len();
        let Progress {
            consumed,
            mut out,
            events,
            peer_closed,
            error,
            ..
        } = c.input(&self.buf[..n], eof, now());
        self.buf.drain(..consumed);
        self.shown = n - consumed;
        let mut moved = consumed > 0 || !out.is_empty();
        if let Some(e) = error {
            self.stats.error.get_or_insert(e);
        }
        if !c.ended() {
            match self.app.step(&mut c, &events, &mut out) {
                Ok(m) => moved |= m,
                Err(e) => panic!("the application failed: {e}"),
            }
        }
        if peer_closed || c.ended() {
            self.ended = true;
        }
        self.state = c.state();
        self.stats.max_state = self.stats.max_state.max(self.state.len());
        self.stats.steps += 1;
        self.stats.kexes = c.kexes();
        self.stats.ciphers = c.ciphers().or(self.stats.ciphers);
        if let Some(u) = c.user() {
            self.stats.user = Some(u.to_owned());
        }
        self.stats.asked = self.app.asked.clone();
        (out, moved)
    }

    /// Input the engine hasn't been shown yet.
    pub fn pending(&self) -> bool {
        self.shown < self.buf.len()
    }
}

/// Serves one TCP connection; the stats at its end.
pub fn serve_one(config: Config, app: App) -> (u16, std::thread::JoinHandle<Stats>) {
    serve_with(config, app, 0)
}

/// [`serve_one`], showing the engine `feed` more bytes per step.
pub fn serve_with(config: Config, app: App, feed: usize) -> (u16, std::thread::JoinHandle<Stats>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let (mut sock, _) = l.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let (mut d, out) = Driver::new(config, app);
        d.feed = feed;
        sock.write_all(&out).unwrap();
        let mut rbuf = vec![0u8; 1 << 16];
        loop {
            let (out, moved) = d.step();
            if !out.is_empty() && sock.write_all(&out).is_err() {
                break;
            }
            if d.ended {
                break;
            }
            if moved || d.pending() {
                continue;
            }
            match sock.read(&mut rbuf) {
                Ok(0) => d.eof = true,
                Ok(n) => d.buf.extend_from_slice(&rbuf[..n]),
                Err(e) => {
                    d.stats
                        .error
                        .get_or_insert(Error::Config(format!("read: {e}")));
                    break;
                }
            }
        }
        let _ = sock.shutdown(std::net::Shutdown::Both);
        d.stats
    });
    (port, h)
}

/// A temporary directory, removed on drop.
pub struct Tmp(pub PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn tmp(name: &str) -> Tmp {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).unwrap();
    let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
    let d = std::env::temp_dir().join(format!("zen-ssh-{name}-{hex}"));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

/// The `ssh` to test against: `ZEN_TEST_SSH`, or the one on PATH.
pub fn ssh_bin() -> PathBuf {
    let p = std::env::var_os("ZEN_TEST_SSH").map_or_else(|| PathBuf::from("ssh"), PathBuf::from);
    let ok = Command::new(&p)
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        ok,
        "these tests need OpenSSH's ssh ({}): install it (apt install openssh-client), or set ZEN_TEST_SSH",
        p.display()
    );
    p
}

/// `ssh-keygen` next to that `ssh`, or on PATH.
pub fn keygen_bin() -> PathBuf {
    let ssh = ssh_bin();
    match ssh.parent() {
        Some(d) if !d.as_os_str().is_empty() => {
            let k = d.join("ssh-keygen");
            if k.exists() {
                return k;
            }
            PathBuf::from("ssh-keygen")
        }
        _ => PathBuf::from("ssh-keygen"),
    }
}

/// A user key made by `ssh-keygen`: its private key file and public key.
pub fn user_key(dir: &Path, name: &str, args: &[&str]) -> (PathBuf, PublicKey) {
    let f = dir.join(name);
    let o = Command::new(keygen_bin())
        .args(["-q", "-N", "", "-C", "test", "-f"])
        .arg(&f)
        .args(args)
        .output()
        .expect("ssh-keygen runs");
    assert!(
        o.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let line = std::fs::read_to_string(f.with_extension("pub")).unwrap();
    (f, PublicKey::from_line(&line).unwrap())
}

/// What an `ssh` run gave.
#[derive(Debug)]
pub struct Run {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs `ssh` against `port` with the key `key` and extra options; `args`
/// after the destination (the command, or `-s` and a subsystem before it).
pub fn ssh(dir: &Path, port: u16, key: &Path, opts: &[&str], args: &[&str], stdin: &[u8]) -> Run {
    let kh = dir.join(format!("known_hosts_{port}"));
    std::fs::write(
        &kh,
        format!("[127.0.0.1]:{port} {}\n", host_key().public_line()),
    )
    .unwrap();
    let mut cmd = Command::new(ssh_bin());
    cmd.arg("-F")
        .arg("/dev/null")
        .args(["-p", &port.to_string(), "-i"])
        .arg(key)
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ConnectTimeout=20",
        ])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", kh.display()));
    for o in opts {
        cmd.args(["-o", o]);
    }
    cmd.arg("-T");
    let (pre, post): (Vec<&str>, Vec<&str>) = match args.first() {
        Some(&"-s") => (vec!["-s"], args[1..].to_vec()),
        _ => (vec![], args.to_vec()),
    };
    cmd.args(pre).arg("test@127.0.0.1").args(post);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("ssh runs");
    let mut si = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    let w = std::thread::spawn(move || {
        let _ = si.write_all(&input);
    });
    let o = child.wait_with_output().unwrap();
    w.join().unwrap();
    Run {
        code: o.status.code(),
        stdout: o.stdout,
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

/// Connects to `port` and copies bytes both ways, after the client's
/// first packet (its KEXINIT) injecting `inject` into the client's stream.
pub fn inject_proxy(port: u16, inject: Vec<u8>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut c2, mut s2) = (c.try_clone().unwrap(), s.try_clone().unwrap());
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut s2, &mut c2);
            let _ = c2.shutdown(std::net::Shutdown::Write);
        });
        // The version line, then the KEXINIT packet (in the clear).
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while c.read_exact(&mut b).is_ok() {
            head.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
        }
        let mut len = [0u8; 4];
        c.read_exact(&mut len).unwrap();
        let mut pkt = vec![0u8; u32::from_be_bytes(len) as usize];
        c.read_exact(&mut pkt).unwrap();
        head.extend_from_slice(&len);
        head.extend_from_slice(&pkt);
        head.extend_from_slice(&inject);
        s.write_all(&head).unwrap();
        let _ = std::io::copy(&mut c, &mut s);
        let _ = s.shutdown(std::net::Shutdown::Write);
    });
    p
}

/// An unencrypted SSH_MSG_IGNORE packet.
pub fn plain_ignore() -> Vec<u8> {
    // packet_length 12: padding length 6, IGNORE with an empty string.
    let mut p = vec![0, 0, 0, 12, 6, 2, 0, 0, 0, 0];
    p.extend_from_slice(&[0; 6]);
    p
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(b).into()
}

pub fn random(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    getrandom::fill(&mut v).unwrap();
    v
}
