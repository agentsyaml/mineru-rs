export interface RunOptions {
  path: string
  output: string
  apiUrl?: string
  method?: 'auto' | 'txt' | 'ocr'
  backend?: 'vlm-http-client' | 'hybrid-http-client' | 'local'
  effort?: 'medium' | 'high' | 'xhigh'
  tier?: 'flash' | 'basic' | 'standard' | 'advanced'
  lang?: string
  url?: string
  start?: number
  end?: number
  formula?: boolean
  table?: boolean
  imageAnalysis?: boolean
  clientSideOutputGeneration?: boolean
}

export interface RunReport {
  warnings: string[]
}

export enum OfficialTier { Flash = 'flash', Basic = 'basic', Standard = 'standard', Advanced = 'advanced' }
export enum OfficialOcrMode { Auto = 'auto', Txt = 'txt', Ocr = 'ocr' }
export enum OfficialWorkerMode { PerDocument = 'per-document', Persistent = 'persistent' }
export enum OfficialRenderFormat {
  Markdown = 'markdown', Html = 'html', Latex = 'latex', Docx = 'docx', Epub = 'epub',
  StructuredContent = 'structured_content', ContentList = 'content_list', ContentListV2 = 'content_list_v2', Pdf = 'pdf',
}
export enum OfficialRenderMode { Default = 'default', Full = 'full' }
export enum OfficialPdfLayout { Auto = 'auto', Original = 'original', Reflow = 'reflow' }

/** Core keeps retained-result (64 MiB) and private-bundle (1 GiB) caps fixed. */
export interface OfficialRuntimeOptions {
  python?: string
  config?: string
  modelDir?: string
  workerMode?: OfficialWorkerMode
  timeoutMs?: number
}
export interface HtmlSourceContext {
  sourceUri?: string
  localResourceRoot?: string
  transportEncoding?: string
}
export interface OfficialVlmConfig {
  engine?: string
  serverUrl?: string
  apiKey?: string
  model?: string
  httpTimeout?: number
  maxConcurrency?: number
}
export interface OfficialParseOptions {
  tier?: OfficialTier
  ocrMode?: OfficialOcrMode
  imageAnalysis?: boolean
  pageRange?: string
  sourceContext?: HtmlSourceContext
  vlmConfig?: OfficialVlmConfig
}
export type OfficialInput = string | { filename: string; data: Buffer | Uint8Array }
export interface OfficialBatchItem { input: OfficialInput; options?: OfficialParseOptions }
export type OfficialAssets = Record<string, Buffer | Uint8Array> | Array<{ path: string; data: Buffer | Uint8Array }>
export interface OfficialLatexDelimiter { left: string; right: string }
export interface OfficialLatexDelimiters { display?: OfficialLatexDelimiter; inline?: OfficialLatexDelimiter }
export interface OfficialRenderOptions {
  mode?: OfficialRenderMode
  assetBaseUrl?: string
  standalone?: boolean
  documentTitle?: string
  assetBasePath?: string
  title?: string
  authors?: string[]
  language?: string
  identifier?: string
  modifiedAt?: string
  layout?: OfficialPdfLayout
  latexDelimiters?: OfficialLatexDelimiters
}
export interface OfficialVersionMetadata { mineruPackage: string; schemaVersion: string; bundleName: string }
export type OfficialErrorKind = 'invalid_options' | 'invalid_input' | 'unsupported_operation' | 'limit_exceeded' | 'runtime_unavailable' | 'upstream' | 'deadline' | 'cancelled' | 'transport'
export interface OfficialErrorShape { kind: OfficialErrorKind; message: string }
export type OfficialRendered =
  | { kind: 'text'; content: string; sidecars: Record<string, Buffer> }
  | { kind: 'json'; value: unknown; sidecars: Record<string, Buffer> }
  | { kind: 'binary'; bytes: Buffer; sidecars: Record<string, Buffer> }
export interface OfficialDataWriter {
  write(path: string, data: Buffer): void | Promise<void>
  writeString(path: string, text: string): void | Promise<void>
}

export class OfficialError extends Error {
  readonly kind: OfficialErrorKind
}
export class OfficialResult {
  private constructor()
  readonly middleJSON: Record<string, unknown>
  readonly modelJSON: unknown | null
  readonly markdown: string
  readonly structuredContent: unknown
  readonly pages: unknown[]
  exportPages(): unknown[]
  readonly assets: Record<string, Buffer>
  readonly warnings: string[]
  readonly versionMetadata: OfficialVersionMetadata
  static fromDict(runtime: OfficialRuntimeOptions | OfficialParser, middleJSON: Record<string, unknown>, assets?: OfficialAssets): Promise<OfficialResult>
  static fromJson(runtime: OfficialRuntimeOptions | OfficialParser, json: string, assets?: OfficialAssets): Promise<OfficialResult>
  toDict(skipDefaults: boolean): Promise<unknown>
  toJson(skipDefaults: boolean): Promise<string>
  render(format: OfficialRenderFormat, options?: OfficialRenderOptions): Promise<OfficialRendered>
  save(root: string, stem: string): void
  saveWithWriter(writer: OfficialDataWriter): Promise<void>
}
export class OfficialParser {
  constructor(runtime?: OfficialRuntimeOptions)
  parseRich(input: OfficialInput, options?: OfficialParseOptions): Promise<OfficialResult>
  parseBatchRich(items: OfficialBatchItem[]): Promise<Array<{ index: number; filename: string; result: OfficialResult | null; error: OfficialErrorShape | null }>>
  fromDict(middleJSON: Record<string, unknown>, assets?: OfficialAssets): Promise<OfficialResult>
  fromJson(json: string, assets?: OfficialAssets): Promise<OfficialResult>
  shutdown(): Promise<void>
}
export declare function parseRich(input: OfficialInput, options?: OfficialParseOptions, runtime?: OfficialRuntimeOptions): Promise<OfficialResult>
export declare function parseBatchRich(items: OfficialBatchItem[], runtime?: OfficialRuntimeOptions): Promise<Array<{ index: number; filename: string; result: OfficialResult | null; error: OfficialErrorShape | null }>>

export declare function canonicalStem(value: string): string
export declare function validatePdfOptions(start: number, end: number | null | undefined, formula: boolean, table: boolean, imageAnalysis: boolean): boolean
export declare function run(options: RunOptions): Promise<RunReport>
/**
 * Parses `options.path` into a private temporary output directory and resolves with the
 * markdown plus warnings. The `output` field of `options` is IGNORED by `parse` (a temp dir
 * is always used and removed before resolving); use `run`/`runCli` for explicit output.
 */
export declare function parse(options: RunOptions): Promise<{ markdown: string; warnings: string[] }>
export declare function runCli(argv: string[]): Promise<number>
