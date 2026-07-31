'use strict'

const assert = require('assert/strict')

const expected = {
  mineru: 'bin/mineru.js',
  'mineru-rs': 'bin/mineru.js',
}

assert.deepEqual(require('../package.json').bin, expected)
assert.deepEqual(require('../package-lock.json').packages[''].bin, expected)
