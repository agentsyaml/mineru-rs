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

// Local copy of api.js locate logic, exercised independently of the full parse flow.
function locateMarkdown(root, stem) {
  const found = []
  const walk = (directory, depth) => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const full = path.join(directory, entry.name)
      if (entry.isDirectory()) {
        if (depth < 2) walk(full, depth + 1)
      } else if (entry.name.endsWith('.md')) {
        found.push(full)
      }
    }
  }
  walk(root, 0)
  if (found.length === 0) throw new Error('parse: no markdown output produced')
  const expected = `${stem}.md`
  for (const file of found) {
    if (path.basename(file) === expected) return file
  }
  return found.reduce((a, b) => (fs.statSync(a).size >= fs.statSync(b).size ? a : b))
}

// Mirrors api.js: the CLI writes `{file_stem}/vlm/{file_stem}.md`, so the stem strips the
// extension before canonicalization.
function stemOf(source) {
  return native.canonicalStem(path.basename(source, path.extname(source)))
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
      fs.writeFileSync(path.join(options.output, `${stemOf(options.path)}.md`), '# parsed')
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
      const stem = stemOf(options.path)
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

async function testLocateLogic() {
  // Root-level `{stem}.md` (stem strips the extension, matching the CLI) is preferred.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mineru-parse-'))
  try {
    fs.writeFileSync(path.join(root, 'other.md'), 'other')
    fs.writeFileSync(path.join(root, 'report.md'), '# parsed')
    assert.equal(locateMarkdown(root, 'report'), path.join(root, 'report.md'))
  } finally {
    fs.rmSync(root, { recursive: true, force: true })
  }

  // `{stem}/` subdirectory layout is found by the depth-2 walk.
  const sub = fs.mkdtempSync(path.join(os.tmpdir(), 'mineru-parse-'))
  try {
    fs.mkdirSync(path.join(sub, 'report', 'vlm'), { recursive: true })
    fs.writeFileSync(path.join(sub, 'report', 'vlm', 'report.md'), '# sub')
    assert.equal(locateMarkdown(sub, 'report'), path.join(sub, 'report', 'vlm', 'report.md'))
  } finally {
    fs.rmSync(sub, { recursive: true, force: true })
  }

  // Without a stem match, the largest `.md` wins.
  const largest = fs.mkdtempSync(path.join(os.tmpdir(), 'mineru-parse-'))
  try {
    fs.writeFileSync(path.join(largest, 'a.md'), 'small')
    fs.writeFileSync(path.join(largest, 'b.md'), 'a much longer markdown body')
    assert.equal(locateMarkdown(largest, 'unknown'), path.join(largest, 'b.md'))
  } finally {
    fs.rmSync(largest, { recursive: true, force: true })
  }

  // Missing markdown is reported.
  const empty = fs.mkdtempSync(path.join(os.tmpdir(), 'mineru-parse-'))
  try {
    assert.throws(() => locateMarkdown(empty, 'report'), /no markdown output produced/)
  } finally {
    fs.rmSync(empty, { recursive: true, force: true })
  }
}

async function main() {
  await testFullFlow()
  await testLocateLogic()
  console.log('node parse: all assertions passed')
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
