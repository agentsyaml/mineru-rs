#!/usr/bin/env node
'use strict'

const { runCli } = require('../api.js')

runCli(process.argv.slice(2)).then(
  (code) => {
    process.exitCode = code
  },
  () => {
    process.stderr.write('mineru: Node command failed\n')
    process.exitCode = 1
  },
)
