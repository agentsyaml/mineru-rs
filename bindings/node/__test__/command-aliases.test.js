'use strict'

const assert = require('assert/strict')
const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const expected = {
  mineru: 'bin/mineru.js',
  'mineru-rs': 'bin/mineru.js',
}

assert.deepEqual(require('../package.json').bin, expected)
assert.deepEqual(require('../package-lock.json').packages[''].bin, expected)

for (const entry of fs.readdirSync(path.join(root, 'npm'), { withFileTypes: true })) {
  if (!entry.isDirectory()) continue
  const manifest = require(path.join(root, 'npm', entry.name, 'package.json'))
  assert.equal(manifest.bin, undefined, `${manifest.name} must not expose command aliases`)
}
