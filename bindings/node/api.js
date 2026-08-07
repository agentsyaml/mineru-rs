'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')
const native = require('./index.js')
const rootManifest = require('./package.json')

const TARGETS = Object.freeze({
  'darwin-x64': { platform: 'darwin', arch: 'x64', helper: 'mineru-office-convert' },
  'darwin-arm64': { platform: 'darwin', arch: 'arm64', helper: 'mineru-office-convert' },
  'linux-x64-gnu': { platform: 'linux', arch: 'x64', helper: 'mineru-office-convert' },
  'linux-arm64-gnu': { platform: 'linux', arch: 'arm64', helper: 'mineru-office-convert' },
  'win32-x64-msvc': { platform: 'win32', arch: 'x64', helper: 'mineru-office-convert.exe' },
  'win32-arm64-msvc': { platform: 'win32', arch: 'arm64', helper: 'mineru-office-convert.exe' },
})

function validArg(value) {
  if (typeof value !== 'string' || value.includes('\0') || value.includes('\ufffd')) return false
  for (let index = 0; index < value.length; index += 1) {
    const unit = value.charCodeAt(index)
    if (unit >= 0xd800 && unit <= 0xdbff) {
      const next = value.charCodeAt(index + 1)
      if (!(next >= 0xdc00 && next <= 0xdfff)) return false
      index += 1
    } else if (unit >= 0xdc00 && unit <= 0xdfff) {
      return false
    }
  }
  return true
}

function helperPath() {
  try {
    const suffix = native._compileTargetSuffix()
    const target = TARGETS[suffix]
    if (!target || process.platform !== target.platform || process.arch !== target.arch) {
      throw new Error('target mismatch')
    }
    if (target.platform === 'linux') {
      const report = process.report && process.report.getReport && process.report.getReport()
      if (!report || !report.header || !report.header.glibcVersionRuntime) {
        throw new Error('GNU libc unavailable')
      }
    }
    const packageName = `@alexsun-top/mineru-${suffix}`
    const helper = require.resolve(`${packageName}/helper`)
    const manifestPath = path.join(path.dirname(helper), 'package.json')
    const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'))
    const info = fs.lstatSync(helper)
    if (
      manifest.name !== packageName ||
      manifest.version !== rootManifest.version ||
      path.basename(helper) !== target.helper ||
      !info.isFile() ||
      info.isSymbolicLink() ||
      (target.platform !== 'win32' && (info.mode & 0o111) !== 0o111)
    ) {
      throw new Error('invalid platform package')
    }
    return helper
  } catch {
    throw new Error('MinerU platform helper validation failed')
  }
}

function canonicalStem(value) {
  return native.canonicalStem(value)
}

function validatePdfOptions(start, end, formula, table, imageAnalysis) {
  return native.validatePdfOptions(start, end, formula, table, imageAnalysis)
}

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
    // The CLI writes `{file_stem}/vlm/{file_stem}.md`; derive the stem the same way (strip
    // the extension from the basename) so the exact-match branch below is the live path.
    const basename = path.basename(options.path, path.extname(options.path))
    const stem = native.canonicalStem(basename)
    const markdown = fs.readFileSync(locateMarkdown(tmp, stem), 'utf8')
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
