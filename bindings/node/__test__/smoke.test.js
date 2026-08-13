'use strict'

const assert = require('assert/strict')
const fs = require('fs')
const http = require('http')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')
const api = require('../api.js')

const PNG = Buffer.from(
  '89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de' +
    '0000000c4944415408d763f8ffff3f0005fe02fe0def46b80000000049454e44ae426082',
  'hex',
)

const crcTable = Array.from({ length: 256 }, (_, value) => {
  let crc = value
  for (let bit = 0; bit < 8; bit += 1) crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0)
  return crc >>> 0
})

function crc32(data) {
  let crc = 0xffffffff
  for (const byte of data) crc = (crc >>> 8) ^ crcTable[(crc ^ byte) & 0xff]
  return (crc ^ 0xffffffff) >>> 0
}

function storedZip(entries) {
  const locals = []
  const central = []
  let offset = 0
  for (const [name, value] of Object.entries(entries)) {
    const filename = Buffer.from(name)
    const data = Buffer.from(value)
    const checksum = crc32(data)
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0)
    local.writeUInt16LE(20, 4)
    local.writeUInt32LE(checksum, 14)
    local.writeUInt32LE(data.length, 18)
    local.writeUInt32LE(data.length, 22)
    local.writeUInt16LE(filename.length, 26)
    locals.push(local, filename, data)

    const entry = Buffer.alloc(46)
    entry.writeUInt32LE(0x02014b50, 0)
    entry.writeUInt16LE(20, 4)
    entry.writeUInt16LE(20, 6)
    entry.writeUInt32LE(checksum, 16)
    entry.writeUInt32LE(data.length, 20)
    entry.writeUInt32LE(data.length, 24)
    entry.writeUInt16LE(filename.length, 28)
    entry.writeUInt32LE(offset, 42)
    central.push(entry, filename)
    offset += local.length + filename.length + data.length
  }
  const directory = Buffer.concat(central)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(Object.keys(entries).length, 8)
  end.writeUInt16LE(Object.keys(entries).length, 10)
  end.writeUInt32LE(directory.length, 12)
  end.writeUInt32LE(offset, 16)
  return Buffer.concat([...locals, directory, end])
}

async function mockApi({ blocked = false, failed = false, files } = {}) {
  let release
  let resultStartedResolve
  const resultStarted = new Promise((resolve) => {
    resultStartedResolve = resolve
  })
  const gate = blocked ? new Promise((resolve) => (release = resolve)) : Promise.resolve()
  const state = { body: Buffer.alloc(0) }
  const archive = storedZip(files || { 'result.txt': 'ok' })
  const server = http.createServer(async (request, response) => {
    if (request.method === 'GET' && request.url === '/health') {
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(JSON.stringify({ status: 'healthy', protocol_version: 2, max_concurrent_requests: 1, processing_window_size: 1 }))
    } else if (request.method === 'POST' && request.url === '/tasks') {
      const chunks = []
      for await (const chunk of request) chunks.push(chunk)
      state.body = Buffer.concat(chunks)
      const base = `http://127.0.0.1:${server.address().port}`
      response.writeHead(202, { 'content-type': 'application/json' })
      response.end(JSON.stringify({ task_id: '1', status_url: `${base}/status/1`, result_url: `${base}/result/1` }))
    } else if (request.method === 'GET' && request.url === '/status/1') {
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(JSON.stringify(failed ? { status: 'failed', message: 'mock detail' } : { status: 'completed' }))
    } else if (request.method === 'GET' && request.url === '/result/1') {
      resultStartedResolve()
      await gate
      response.writeHead(200, { 'content-type': 'application/zip', 'content-length': archive.length })
      response.end(archive)
    } else {
      response.writeHead(404)
      response.end()
    }
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    state,
    resultStarted,
    release: () => release && release(),
    close: () => new Promise((resolve) => server.close(resolve)),
  }
}

async function withTemp(run) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mineru-node-test-'))
  try {
    return await run(root)
  } finally {
    fs.rmSync(root, { recursive: true, force: true })
  }
}

function child(source) {
  return spawnSync(process.execPath, ['-e', source], { cwd: path.join(__dirname, '..') })
}

async function main() {
  const started = Date.now()
  assert.deepEqual(Object.keys(api).sort(), ['canonicalStem', 'parse', 'run', 'runCli', 'validatePdfOptions'])
  assert.equal('signal' in api.run, false)
  assert.equal(api.canonicalStem('a bad/pdf'), 'a bad_pdf')
  assert.equal(api.canonicalStem(''), 'document')
  assert.throws(() => api.canonicalStem('con'), Error)
  assert.equal(api.validatePdfOptions(0, null, true, true, true), true)
  assert.throws(() => api.validatePdfOptions(5, 2, true, true, true), Error)

  await withTemp(async (root) => {
    const input = path.join(root, 'inputs')
    const output = path.join(root, 'output')
    fs.mkdirSync(input)
    fs.mkdirSync(output)
    fs.writeFileSync(path.join(input, 'input.png'), PNG)
    fs.writeFileSync(path.join(input, 'ignored.txt'), 'ignored')
    const mock = await mockApi()
    try {
      const report = await api.run({
        path: input,
        output,
        apiUrl: mock.url,
        method: 'ocr',
        effort: 'high',
        lang: 'korean',
        url: 'http://model.invalid',
        start: 3,
        end: 4,
        formula: false,
        table: false,
        imageAnalysis: false,
      })
      assert.deepEqual(Object.keys(report), ['warnings'])
      assert(report.warnings.some((warning) => warning.includes('unsupported input')))
      assert.equal(fs.readFileSync(path.join(output, 'result.txt'), 'utf8'), 'ok')
      const body = mock.state.body.toString()
      for (const value of ['name="lang_list"\r\n\r\nkorean', 'name="effort"\r\n\r\nhigh', 'name="parse_method"\r\n\r\nocr', 'name="start_page_id"\r\n\r\n3', 'name="end_page_id"\r\n\r\n4']) assert(body.includes(value), value)
    } finally {
      await mock.close()
    }
  })

  await withTemp(async (root) => {
    const input = path.join(root, 'input.png')
    fs.writeFileSync(input, PNG)
    const failedOutput = path.join(root, 'failed')
    fs.mkdirSync(failedOutput)
    const mock = await mockApi({ failed: true })
    try {
      await assert.rejects(api.run({ path: input, output: failedOutput, apiUrl: mock.url }), /1 API task\(s\) failed: task#1 \[input\].*mock detail/)
    } finally {
      await mock.close()
    }
    const untouched = path.join(root, 'untouched')
    await assert.rejects(api.run({ path: path.join(root, 'missing.png'), output: untouched, method: 'invalid' }), /unsupported method: invalid/)
    assert.equal(fs.existsSync(untouched), false)
  })

  await withTemp(async (root) => {
    const input = path.join(root, 'input.png')
    const output = path.join(root, 'output')
    fs.writeFileSync(input, PNG)
    fs.mkdirSync(output)
    const mock = await mockApi({ blocked: true })
    try {
      let finished = false
      const running = api.run({ path: input, output, apiUrl: mock.url }).finally(() => (finished = true))
      await mock.resultStarted
      const timer = Date.now()
      await new Promise((resolve) => setTimeout(resolve, 10))
      assert(Date.now() - timer < 200)
      assert.equal(finished, false)
      mock.release()
      await running
    } finally {
      mock.release()
      await mock.close()
    }
  })

  const jsInvalid = child("require('./api.js').runCli(['\\0']).then(c=>process.exit(c===2?0:3))")
  assert.equal(jsInvalid.status, 0)
  assert.equal(jsInvalid.stdout.length, 0)
  assert.equal(jsInvalid.stderr.toString(), 'error: invalid Node CLI argument encoding\n')
  const nativeInvalid = child(`const n=require('./index.js'),h=require('path').join(__dirname,'mineru-office-convert');n._runCli(['\\ud800'],h).then(c=>process.exit(c===2?0:3))`)
  assert.equal(nativeInvalid.status, 0)
  assert.equal(nativeInvalid.stderr.toString(), 'error: invalid Node CLI argument encoding\n')

  console.log(`node source: all assertions passed in ${Date.now() - started}ms`)
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
