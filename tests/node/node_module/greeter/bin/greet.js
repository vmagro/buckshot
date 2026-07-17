#!/usr/bin/env node
const process = require('process');
const { green } = require("colors");
const pc = require("picocolors");
console.log(green("hello from an in-tree node_module!"));
console.log(pc.blue("...composed with a cross-cell npm_archive dep!"));
console.log(`running via node: ${process.argv0}`);