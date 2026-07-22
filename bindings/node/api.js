'use strict'

const fs = require('fs')
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

async function run(options) {
  return native._run(options, helperPath())
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

module.exports = { canonicalStem, validatePdfOptions, run, runCli }
