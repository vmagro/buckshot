#!/usr/bin/env node
const { green } = require("colors");
const pc = require("picocolors");
console.log(green("hello from an in-tree node_module!"));
console.log(pc.blue("...composed with a cross-cell npm_archive dep!"));
