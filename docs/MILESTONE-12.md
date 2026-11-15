# Milestone 12: zen-net, a post-quantum private network between members' devices (placeholder)

A placeholder added while planning milestone 10 ([DESIGN-3 §6](DESIGN-3.md) item 12). It comes after milestone 11.

* **Feature:** members' devices form small private networks (a virtual LAN) where they see each other by address, as WireGuard peers do, with zen-serve as the rendezvous and, when needed, a blind relay.
* **Permissions:** a network is a zen-db object. Grants `net.join` and `net.admin`, a table of address assignments updated by compare-and-set, and optional per-peer rules. The server learns who belongs to which network, but never sees the traffic.
* **The handshake:** devices already hold hybrid ML-DSA-65 certificates and X-Wing keys. Peers verify each other's certificates against the ACL, run a post-quantum KEM in both directions, and rekey every few minutes.
* **The data plane, proposed:** WireGuard (the kernel's, or boringtun in userspace), given a post-quantum pre-shared key from that handshake and rotated, as Rosenpass does: hybrid security on audited code. Alternatives: a data plane of our own over UDP, or a userspace network stack that needs no root.
* **Getting packets through:** UDP hole punching with zen-serve as the rendezvous (the server then sees peers' public addresses); otherwise a blind relay of the encrypted packets through zen-serve, which costs server bandwidth and needs quotas.
* **First, a spike** (as milestone 9's step 9.1): how often hole punching works, WireGuard with the key against userspace throughput, and what needs root. Linux first.

**The rest is planned when the milestone starts.**
