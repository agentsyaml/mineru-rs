'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')
const native = require('./index.js')

const OfficialTier = Object.freeze({ Flash: 'flash', Basic: 'basic', Standard: 'standard', Advanced: 'advanced' })
const OfficialOcrMode = Object.freeze({ Auto: 'auto', Txt: 'txt', Ocr: 'ocr' })
const OfficialWorkerMode = Object.freeze({ PerDocument: 'per-document', Persistent: 'persistent' })
const OfficialRenderFormat = Object.freeze({
  Markdown: 'markdown', Html: 'html', Latex: 'latex', Docx: 'docx', Epub: 'epub',
  StructuredContent: 'structured_content', ContentList: 'content_list', ContentListV2: 'content_list_v2', Pdf: 'pdf',
})
const OfficialRenderMode = Object.freeze({ Default: 'default', Full: 'full' })
const OfficialPdfLayout = Object.freeze({ Auto: 'auto', Original: 'original', Reflow: 'reflow' })

class OfficialError extends Error {
  constructor(kind, message) {
    super(message)
    this.name = 'OfficialError'
    this.kind = kind
  }
}

function wrapOfficialError(error) {
  if (error instanceof OfficialError) return error
  const match = /^([a-z_]+): (.*)$/s.exec(String(error && error.message))
  return match ? new OfficialError(match[1], match[2]) : error
}

function plainObject(value, label) {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new TypeError(`${label} must be an object`)
  return value
}

function onlyKeys(value, keys, label) {
  plainObject(value, label)
  for (const key of Object.keys(value)) if (!keys.includes(key)) throw new TypeError(`unknown ${label} field: ${key}`)
}

function nativeInput(input) {
  if (typeof input === 'string') return { path: input }
  onlyKeys(input, ['filename', 'data'], 'input')
  if (typeof input.filename !== 'string' || input.filename.length === 0) throw new TypeError('input.filename is required')
  if (!(input.data instanceof Uint8Array)) throw new TypeError('input.data must be a Buffer or Uint8Array')
  return { filename: input.filename, data: Buffer.from(input.data) }
}

function nativeAssets(assets) {
  if (assets === undefined) return []
  if (Array.isArray(assets)) {
    return assets.map(({ path: name, data }) => ({ path: name, data: Buffer.from(data) }))
  }
  plainObject(assets, 'assets')
  return Object.entries(assets).map(([path, data]) => ({ path, data: Buffer.from(data) }))
}

function nativeParseOptions(options) {
  if (options === undefined) return undefined
  onlyKeys(options, ['tier', 'ocrMode', 'imageAnalysis', 'pageRange', 'sourceContext', 'vlmConfig'], 'parse options')
  const wire = {}
  for (const [source, target] of [['tier', 'tier'], ['ocrMode', 'ocr_mode'], ['imageAnalysis', 'image_analysis'], ['pageRange', 'page_range']]) {
    if (options[source] !== undefined) wire[target] = options[source]
  }
  if (options.sourceContext !== undefined) {
    onlyKeys(options.sourceContext, ['sourceUri', 'localResourceRoot', 'transportEncoding'], 'sourceContext')
    wire.source_context = {
      source_uri: options.sourceContext.sourceUri,
      local_resource_root: options.sourceContext.localResourceRoot,
      transport_encoding: options.sourceContext.transportEncoding,
    }
  }
  if (options.vlmConfig !== undefined) {
    onlyKeys(options.vlmConfig, ['engine', 'serverUrl', 'apiKey', 'model', 'httpTimeout', 'maxConcurrency'], 'vlmConfig')
    wire.vlm_config = {
      engine: options.vlmConfig.engine, server_url: options.vlmConfig.serverUrl,
      api_key: options.vlmConfig.apiKey, model: options.vlmConfig.model,
      http_timeout: options.vlmConfig.httpTimeout, max_concurrency: options.vlmConfig.maxConcurrency,
    }
  }
  return JSON.stringify(wire)
}

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

function nativeRuntimeOptions(options) {
  if (options === undefined) return undefined
  onlyKeys(options, ['python', 'config', 'modelDir', 'workerMode', 'timeoutMs'], 'runtime options')
  return { ...options }
}

function nativeRenderOptions(options) {
  if (options === undefined) return undefined
  const fields = {
    mode: 'mode', assetBaseUrl: 'asset_base_url', standalone: 'standalone',
    documentTitle: 'document_title', assetBasePath: 'asset_base_path', title: 'title',
    authors: 'authors', language: 'language', identifier: 'identifier',
    modifiedAt: 'modified_at', layout: 'layout', latexDelimiters: 'latex_delimiters',
  }
  onlyKeys(options, Object.keys(fields), 'render options')
  const wire = {}
  for (const [source, target] of Object.entries(fields)) {
    if (options[source] === undefined) continue
    if (source === 'latexDelimiters') {
      onlyKeys(options[source], ['display', 'inline'], 'latexDelimiters')
      wire[target] = options[source]
    } else wire[target] = options[source]
  }
  return JSON.stringify(wire)
}

const resultToken = Symbol('official-result')

class OfficialResult {
  constructor(token, handle) {
    if (token !== resultToken) throw new TypeError('OfficialResult instances are created by OfficialParser')
    Object.defineProperty(this, '_native', { value: handle })
  }

  static async fromDict(runtimeOptions, middleJSON, assets = {}) {
    if (runtimeOptions instanceof OfficialParser) return runtimeOptions.fromDict(middleJSON, assets)
    const parser = new OfficialParser(runtimeOptions)
    try { return await parser.fromDict(middleJSON, assets) } finally { await parser.shutdown() }
  }

  static async fromJson(runtimeOptions, json, assets = {}) {
    if (runtimeOptions instanceof OfficialParser) return runtimeOptions.fromJson(json, assets)
    const parser = new OfficialParser(runtimeOptions)
    try { return await parser.fromJson(json, assets) } finally { await parser.shutdown() }
  }

  get middleJSON() { return JSON.parse(this._native.middleJson) }
  get modelJSON() { return this._native.modelJson == null ? null : JSON.parse(this._native.modelJson) }
  get markdown() { return this._native.markdown }
  get structuredContent() { return JSON.parse(this._native.structuredContent) }
  get pages() { return JSON.parse(this._native.pages) }
  get warnings() { return this._native.warnings }
  get versionMetadata() { return this._native.versionMetadata }
  get assets() { return Object.fromEntries(this._native.assets.map((asset) => [asset.path, Buffer.from(asset.data)])) }
  exportPages() { return JSON.parse(this._native.exportPages()) }

  async toDict(skipDefaults) { return JSON.parse(await this._native.toDict(skipDefaults)) }
  async toJson(skipDefaults) { return this._native.toJson(skipDefaults) }

  async render(format, options) {
    try {
      const rendered = await this._native.render(format, nativeRenderOptions(options))
      const sidecars = Object.fromEntries(rendered.sidecars.map((asset) => [asset.path, Buffer.from(asset.data)]))
      if (rendered.kind === 'text') return { kind: 'text', content: rendered.text, sidecars }
      if (rendered.kind === 'json') return { kind: 'json', value: JSON.parse(rendered.json), sidecars }
      if (rendered.kind === 'binary') return { kind: 'binary', bytes: Buffer.from(rendered.data), sidecars }
      throw new Error('invalid native render kind')
    } catch (error) { throw wrapOfficialError(error) }
  }

  save(root, stem) {
    try { return this._native.save(root, stem) } catch (error) { throw wrapOfficialError(error) }
  }

  async saveWithWriter(writer) {
    if (!writer || typeof writer.write !== 'function' || typeof writer.writeString !== 'function') {
      throw new TypeError('writer must implement write(path, Buffer) and writeString(path, text)')
    }
    let files
    try { files = this._native.writerFiles() } catch (error) { throw wrapOfficialError(error) }
    for (const file of files) {
      if (file.text !== undefined && file.text !== null) await writer.writeString(file.path, file.text)
      else await writer.write(file.path, Buffer.from(file.data))
    }
  }
}

class OfficialParser {
  constructor(options) {
    if (typeof native.NativeOfficialParser !== 'function') {
      throw new Error('official Node SDK requires rebuilding the native binding')
    }
    try {
      Object.defineProperty(this, '_native', { value: new native.NativeOfficialParser(nativeRuntimeOptions(options)) })
    } catch (error) { throw wrapOfficialError(error) }
  }

  async parseRich(source, options) {
    try {
      const result = await this._native.parseRich(nativeInput(source), nativeParseOptions(options))
      return new OfficialResult(resultToken, result)
    } catch (error) { throw wrapOfficialError(error) }
  }

  async parseBatchRich(items) {
    if (!Array.isArray(items)) throw new TypeError('items must be an array')
    try {
      const results = await this._native.parseBatchRich(items.map((item) => {
        onlyKeys(item, ['input', 'options'], 'batch item')
        return { input: nativeInput(item.input), options: nativeParseOptions(item.options) }
      }))
      return results.map((item) => ({
        index: item.index,
        filename: item.filename,
        result: (() => { const result = item.takeResult(); return result == null ? null : new OfficialResult(resultToken, result) })(),
        error: item.errorKind == null ? null : new OfficialError(item.errorKind, item.errorMessage),
      }))
    } catch (error) { throw wrapOfficialError(error) }
  }

  async fromDict(middleJSON, assets = {}) {
    try { return new OfficialResult(resultToken, await this._native.fromDict(JSON.stringify(middleJSON), nativeAssets(assets))) }
    catch (error) { throw wrapOfficialError(error) }
  }

  async fromJson(json, assets = {}) {
    if (typeof json !== 'string') throw new TypeError('json must be a string')
    try { return new OfficialResult(resultToken, await this._native.fromJson(json, nativeAssets(assets))) }
    catch (error) { throw wrapOfficialError(error) }
  }

  async shutdown() {
    try { await this._native.shutdown() } catch (error) { throw wrapOfficialError(error) }
  }
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

async function parseRich(input, options, runtimeOptions) {
  const parser = new OfficialParser(runtimeOptions)
  try { return await parser.parseRich(input, options) } finally { await parser.shutdown() }
}

async function parseBatchRich(items, runtimeOptions) {
  const parser = new OfficialParser(runtimeOptions)
  try { return await parser.parseBatchRich(items) } finally { await parser.shutdown() }
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

module.exports = {
  canonicalStem, validatePdfOptions, run, parse, runCli,
  OfficialParser, OfficialResult, OfficialError,
  OfficialTier, OfficialOcrMode, OfficialWorkerMode, OfficialRenderFormat, OfficialRenderMode, OfficialPdfLayout,
  parseRich, parseBatchRich,
}
