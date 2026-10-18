'use strict'

const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const test = require('node:test')
const { OfficialParser, OfficialResult, OfficialRenderFormat, OfficialTier, OfficialWorkerMode } = require('../api.js')

const python = process.env.MINERU_OFFICIAL_PYTHON
const png = Buffer.from('89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de0000000c4944415408d763f8ffff3f0005fe02fe0def46b80000000049454e44ae426082', 'hex')

function mhtml() {
  const image = png.toString('base64')
  return [
    'MIME-Version: 1.0', 'Content-Type: multipart/related; boundary="sdk-boundary"; type="text/html"', '',
    '--sdk-boundary', 'Content-Type: text/html; charset="utf-8"', 'Content-Location: file:///sample/index.html', '',
    '<html><body><p>Node rich SDK</p><img src="cid:sdk-image"></body></html>',
    '--sdk-boundary', 'Content-Type: image/png', 'Content-Transfer-Encoding: base64',
    'Content-ID: <sdk-image>', '', image, '--sdk-boundary--', '',
  ].join('\r\n')
}

test('native rich Node SDK lifecycle with external MinerU 4.0.12', { skip: !python }, async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mineru-node-official-'))
  t.after(() => fs.rmSync(root, { recursive: true, force: true }))
  const csv = path.join(root, 'sample.csv')
  const mail = path.join(root, 'sample.mhtml')
  fs.writeFileSync(csv, 'name,value\nalpha,1\n')
  fs.writeFileSync(mail, mhtml())
  for (const workerMode of [OfficialWorkerMode.PerDocument, OfficialWorkerMode.Persistent]) {
    const parser = new OfficialParser({ python, workerMode, timeoutMs: 120000 })
    const result = await parser.parseRich(csv, { tier: OfficialTier.Flash, imageAnalysis: false })
    assert.match(result.markdown, /alpha/)
    assert.equal(result.versionMetadata.schemaVersion, '2.0')
    await parser.shutdown()
  }
  const parser = new OfficialParser({ python, workerMode: OfficialWorkerMode.PerDocument, timeoutMs: 120000 })
  try {
    const batch = await parser.parseBatchRich([
      { input: csv },
      { input: { filename: 'bad.txt', data: Buffer.from('unsupported') } },
    ])
    assert.deepEqual(batch.map(({ index }) => index), [0, 1])
    assert.match(batch[0].result.markdown, /alpha/)
    assert.equal(batch[1].result, null)
    assert.equal(batch[1].error.kind, 'invalid_input')

    const parsed = await parser.parseRich(mail, { tier: OfficialTier.Flash, imageAnalysis: false })
    assert.match(parsed.markdown, /Node rich SDK/)
    assert(Object.keys(parsed.assets).some((name) => name.startsWith('images/')))
    const restored = await OfficialResult.fromDict(parser, parsed.middleJSON, parsed.assets)
    const roundTrip = await restored.toJson(false)
    const fromJson = await OfficialResult.fromJson(parser, roundTrip, restored.assets)
    assert.equal((await fromJson.toDict(false)).schema_version, '2.0')

    const rendered = await Promise.all([
      [OfficialRenderFormat.Markdown, 'text'], [OfficialRenderFormat.Html, 'text'],
      [OfficialRenderFormat.Latex, 'text'], [OfficialRenderFormat.StructuredContent, 'json'],
      [OfficialRenderFormat.ContentList, 'json'], [OfficialRenderFormat.ContentListV2, 'json'],
      [OfficialRenderFormat.Docx, 'binary'], [OfficialRenderFormat.Epub, 'binary'],
      [OfficialRenderFormat.Pdf, 'binary'],
    ].map(async ([format, kind]) => {
      const value = await fromJson.render(format)
      assert.equal(value.kind, kind, format)
      return value
    }))
    const [markdown, , , structured, , , binary] = rendered
    assert(Object.keys(markdown.sidecars).length > 0)
    assert(Buffer.isBuffer(binary.bytes))
    await fromJson.save(root, 'saved')
    assert(fs.statSync(path.join(root, 'saved', 'hybrid-v4', 'middle_json.json')).isFile())
    let callbacks = 0
    await assert.rejects(fromJson.saveWithWriter({
      writeString() { callbacks += 1; throw new Error('intentional writer failure') },
      write() { callbacks += 1 },
    }), /intentional writer failure/)
    assert.equal(callbacks, 1)
    const carrier = fromJson.middleJSON.pages.flatMap((page) => page.blocks).flatMap((block) => block.content || [])
      .find((block) => block.image_path)
    assert(carrier, 'fixture has a typed image carrier')
    const partial = await parser.fromDict(fromJson.middleJSON, {})
    callbacks = 0
    await assert.rejects(partial.saveWithWriter({
      writeString() { callbacks += 1 }, write() { callbacks += 1 },
    }))
    assert.equal(callbacks, 0, 'missing assets fail before the first writer callback')
  } finally {
    await parser.shutdown()
  }
})
