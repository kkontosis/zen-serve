#!/usr/bin/env node
// git-remote-zen (spec/git.md §7): git runs it for zen:// and zen+http:// URLs.
import { helper } from './helper.js';
import { main } from './main.js';

main(helper);
