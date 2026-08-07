export interface RunOptions {
  path: string
  output: string
  apiUrl?: string
  method?: 'auto' | 'txt' | 'ocr'
  backend?: 'vlm-http-client'
  effort?: 'medium' | 'high'
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
