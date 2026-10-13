'use strict'

const assert = require('assert/strict')
const native = require('../index.js')
const api = require('../api.js')

const calls = {}
class MockResult {
  get middleJson() { return JSON.stringify({ pages: [] }) }
  get modelJson() { return null }
  get markdown() { return '# owned' }
  get structuredContent() { return JSON.stringify({ pages: [] }) }
  get pages() { return JSON.stringify([]) }
  exportPages() { return JSON.stringify([]) }
  get assets() { return [{ path: 'images/a.png', data: Buffer.from([1, 2]) }] }
  get warnings() { return ['warning'] }
  get versionMetadata() { return { mineruPackage: '4.0.10', schemaVersion: '2.0', bundleName: 'hybrid-v4' } }
  async toDict(skip) { calls.skip = skip; return '{"pages":[]}' }
  async toJson(skip) { calls.skip = skip; return '{"pages":[]}' }
  async render(format, options) {
    calls.render = [format, options]
    return { kind: 'json', json: '{"ok":true}', sidecars: this.assets }
  }
  save(root, stem) { calls.save = [root, stem] }
  writerFiles() { return [{ path: 'markdown.md', text: '# ok' }, { path: 'images/a.png', data: Buffer.from([1]) }] }
}
class MockBatchResult {
  constructor(index, filename, failed) { this.index = index; this.filename = filename; this.result = failed ? null : new MockResult(); this.errorKind = failed ? 'invalid_input' : null; this.errorMessage = failed ? 'invalid input' : null }
  takeResult() { const result = this.result; this.result = null; return result }
}
class MockParser {
  constructor(options) { calls.runtime = options }
  async parseRich(input, options) { calls.input = input; calls.parseOptions = JSON.parse(options); return new MockResult() }
  async parseBatchRich(items) { calls.batch = items; return items.map((item, i) => new MockBatchResult(i, item.input.filename || item.input.path, i === 1)) }
  async fromDict(json, assets) { calls.restore = [JSON.parse(json), assets]; return new MockResult() }
  async fromJson(json, assets) { calls.restoreJSON = [json, assets]; return new MockResult() }
  async shutdown() { calls.shutdown = true }
}

async function main() {
  const old = native.NativeOfficialParser
  native.NativeOfficialParser = MockParser
  try {
    const parser = new api.OfficialParser({ python: '/private/python', config: '/private/config', modelDir: '/private/models', workerMode: 'persistent', timeoutMs: 5000 })
    const bytes = Uint8Array.from([9, 1, 2, 8]).subarray(1, 3)
    const result = await parser.parseRich({ filename: 'input.html', data: bytes }, {
      tier: api.OfficialTier.Flash, ocrMode: api.OfficialOcrMode.Ocr, imageAnalysis: false,
      pageRange: '1,3-5', sourceContext: { sourceUri: 'file:///input.html', localResourceRoot: '/trusted', transportEncoding: 'utf-8' },
      vlmConfig: { engine: 'auto', serverUrl: 'https://model.invalid', apiKey: 'secret', model: 'm', httpTimeout: 9, maxConcurrency: 2 },
    })
    assert.deepEqual(calls.input.data, Buffer.from([1, 2]))
    assert.deepEqual(calls.runtime, { python: '/private/python', config: '/private/config', modelDir: '/private/models', workerMode: 'persistent', timeoutMs: 5000 })
    assert.deepEqual(calls.parseOptions, {
      tier: 'flash', ocr_mode: 'ocr', image_analysis: false, page_range: '1,3-5',
      source_context: { source_uri: 'file:///input.html', local_resource_root: '/trusted', transport_encoding: 'utf-8' },
      vlm_config: { engine: 'auto', server_url: 'https://model.invalid', api_key: 'secret', model: 'm', http_timeout: 9, max_concurrency: 2 },
    })
    assert.equal(result.markdown, '# owned')
    assert.deepEqual(result.pages, [])
    assert.deepEqual(result.exportPages(), [])
    assert.equal(result.assets['images/a.png'].toString('hex'), '0102')
    assert.equal(JSON.stringify(result).includes('/private'), false)
    assert.deepEqual(await result.toDict(false), { pages: [] })
    assert.deepEqual(await result.render(api.OfficialRenderFormat.ContentList, {}), {
      kind: 'json', value: { ok: true }, sidecars: { 'images/a.png': Buffer.from([1, 2]) },
    })
    result.save('/output', 'stem')
    assert.deepEqual(calls.save, ['/output', 'stem'])
    const batch = await parser.parseBatchRich([
      { input: '/one.pdf' },
      { input: { filename: 'two.csv', data: Buffer.from('two') }, options: { tier: api.OfficialTier.Flash } },
    ])
    assert.deepEqual(batch.map((item) => item.index), [0, 1])
    assert.equal(batch[0].result.markdown, '# owned')
    assert.equal(batch[1].error.kind, 'invalid_input')
    assert.deepEqual(calls.batch.map((item) => item.options), [undefined, '{"tier":"flash"}'])
    await parser.fromDict({ pages: [] }, { 'images/a.png': Buffer.from([1]) })
    assert.deepEqual(calls.restore[1][0], { path: 'images/a.png', data: Buffer.from([1]) })
    await parser.fromJson('{"pages":[]}', {})
    assert.equal(calls.restoreJSON[0], '{"pages":[]}')

    let writes = 0
    await assert.rejects(result.saveWithWriter({
      writeString() { writes += 1; throw new Error('writer failed') },
      write() { writes += 1 },
    }), /writer failed/)
    assert.equal(writes, 1)
    const files = []
    await result.saveWithWriter({ writeString: (name, text) => files.push([name, text]), write: (name, data) => files.push([name, Buffer.from(data)]) })
    assert.deepEqual(files, [['markdown.md', '# ok'], ['images/a.png', Buffer.from([1])]])
    const oldFiles = MockResult.prototype.writerFiles
    MockResult.prototype.writerFiles = () => { throw new Error('limit_exceeded: invalid asset') }
    writes = 0
    await assert.rejects(result.saveWithWriter({ writeString() { writes += 1 }, write() { writes += 1 } }), (error) => error.kind === 'limit_exceeded')
    assert.equal(writes, 0, 'core prevalidation precedes the first JS writer callback')
    MockResult.prototype.writerFiles = oldFiles

    const oldParse = MockParser.prototype.parseRich
    MockParser.prototype.parseRich = async () => { throw new Error('invalid_input: rejected') }
    await assert.rejects(parser.parseRich('/bad.pdf'), (error) => error.kind === 'invalid_input')
    MockParser.prototype.parseRich = oldParse
    await parser.shutdown()
  } finally {
    native.NativeOfficialParser = old
  }
  console.log('node official sdk: all assertions passed')
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
