'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')
const native = require('./index.js')

// Type gate only: encoding validity (NUL, lone surrogates) is owned by the Rust
// side (`_runCli` validates UTF-16 before converting to OsString).
function validArg(value) {
  return typeof value === 'string'
}

// The `mineru-office-convert` helper binary is not bundled in the npm package, so this
// always returns a path that does not exist. The Rust core maps the resulting spawn
// failure to `OfficeConvertError::Unavailable` ("office conversion is unavailable"),
// keeping PDF processing working while office inputs fail with a clear error.
function helperPath() {
  return path.join(__dirname, 'mineru-office-convert' + (process.platform === 'win32' ? '.exe' : ''))
}

function canonicalStem(value) {
  return native.canonicalStem(value)
}

function validatePdfOptions(start, end, formula, table, imageAnalysis) {
  return native.validatePdfOptions(start, end, formula, table, imageAnalysis)
}

async function locateMarkdown(root, stem) {
  if (typeof native._locateMarkdown === 'function') {
    return native._locateMarkdown(String(root), stem)
  }
  // Fallback for prebuilt native binaries without the locator: reproduce the
  // retired depth-2 walk so `parse` keeps working with old `.node` artifacts.
  return legacyLocateMarkdown(root, stem)
}

function legacyLocateMarkdown(root, stem) {
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

async function run(options) {
  return native._run(options, helperPath())
}

// Parses `options.path` in a private temporary output directory and returns the markdown
// plus collected warnings. NOTE: any user-supplied `options.output` is intentionally ignored
// here — `parse` always uses a temp dir (which is removed before returning), mirroring the
// Python facade. Use `runCli` or `run` for explicit output control.
async function parse(options) {
  if (typeof options !== 'object' || options === null || typeof options.path !== 'string' || options.path.trim() === '') {
    throw new Error('parse: options.path is required and must be a non-empty string')
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'mineru-'))
  try {
    const report = await native._run({ ...options, output: tmp }, helperPath())
    // The runner publishes `{file_stem}/vlm/{file_stem}.md`; the Rust locator knows
    // every layout, so only a missing file still needs the friendly error here.
    const basename = path.basename(options.path, path.extname(options.path))
    const stem = native.canonicalStem(basename)
    const markdownPath = await locateMarkdown(tmp, stem)
    if (markdownPath === null || markdownPath === undefined) {
      throw new Error('parse: no markdown output produced')
    }
    const markdown = fs.readFileSync(markdownPath, 'utf8')
    return { markdown, warnings: report.warnings }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true })
  }
}

async function runCli(argv) {
  if (!Array.isArray(argv) || !argv.every(validArg)) {
    process.stderr.write('error: invalid Node CLI argument encoding\n')
    return 2
  }
  try {
    return await native._runCli(argv, helperPath())
  } catch {
    process.stderr.write('mineru: Node command failed\n')
    return 1
  }
}

module.exports = { canonicalStem, validatePdfOptions, run, parse, runCli }
