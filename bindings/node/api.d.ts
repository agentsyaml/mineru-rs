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
export declare function runCli(argv: string[]): Promise<number>
