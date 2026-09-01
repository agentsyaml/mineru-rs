'use strict'

const assert = require('assert/strict')
const fs = require('fs')
const os = require('os')
const path = require('path')
const api = require('../api.js')
const native = require('../index.js')

function patchRun(replacement) {
  const original = native._run
  native._run = replacement
  return () => {
    native._run = original
  }
}

function tempSource() {
  const source = path.join(os.tmpdir(), `mineru-parse-in-${process.pid}.pdf`)
  fs.writeFileSync(source, 'fake')
  return source
}

async function testFullFlow() {
  const calls = []

  {
    const restore = patchRun(async (options) => {
      calls.push(options.output)
      fs.mkdirSync(options.output, { recursive: true })
      const stem = path.basename(options.path, path.extname(options.path))
      fs.writeFileSync(path.join(options.output, `${stem}.md`), '# parsed')
      return { warnings: ['warn'] }
    })
    try {
      const source = tempSource()
      try {
        const result = await api.parse({ path: source, method: 'ocr' })
        assert.deepEqual(result, { markdown: '# parsed', warnings: ['warn'] })
        assert.equal(calls.length, 1)
        assert.equal(fs.existsSync(calls[0]), false, 'temp output dir was not cleaned up')
      } finally {
        fs.rmSync(source, { force: true })
      }
    } finally {
      restore()
    }
  }

  {
    // Markdown inside a `{stem}/` subdirectory is located via the depth-2 walk.
    const restore = patchRun(async (options) => {
      calls.push(options.output)
      const stem = path.basename(options.path, path.extname(options.path))
      fs.mkdirSync(path.join(options.output, stem, 'vlm'), { recursive: true })
      fs.writeFileSync(path.join(options.output, stem, 'vlm', `${stem}.md`), '# sub')
      return { warnings: [] }
    })
    try {
      const source = tempSource()
      try {
        const result = await api.parse({ path: source })
        assert.equal(result.markdown, '# sub')
        assert.deepEqual(result.warnings, [])
        assert.equal(fs.existsSync(calls[1]), false, 'temp output dir was not cleaned up')
      } finally {
        fs.rmSync(source, { force: true })
      }
    } finally {
      restore()
    }
  }

  {
    // No markdown produced: parse rejects and the temp dir is still removed.
    const restore = patchRun(async (options) => {
      calls.push(options.output)
      fs.mkdirSync(options.output, { recursive: true })
      return { warnings: [] }
    })
    try {
      const source = tempSource()
      try {
        await assert.rejects(api.parse({ path: source }), /no markdown output produced/)
        assert.equal(fs.existsSync(calls[2]), false, 'temp output dir was not cleaned up on failure')
      } finally {
        fs.rmSync(source, { force: true })
      }
    } finally {
      restore()
    }
  }

  {
    // Invalid inputs are rejected with a clear error before any work (no temp dir is made).
    for (const bad of [null, undefined, {}, { path: '' }, { path: '   ' }]) {
      await assert.rejects(api.parse(bad), /options\.path is required/)
    }
  }
}

async function main() {
  await testFullFlow()
  console.log('node parse: all assertions passed')
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
