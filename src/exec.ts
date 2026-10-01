export interface RunResult {
  exitCode: number;
  /** Only set when output was captured instead of streamed. */
  output?: string;
  /** The end of stderr, when it was watched. */
  stderr?: string;
}

export interface RunOptions {
  /** Capture all output instead of streaming it. */
  capture?: boolean;
  /** Stream stderr through and keep a copy, so a failure can be diagnosed. */
  watchStderr?: boolean;
}

const STDERR_KEPT = 4000;

/** Run shell steps in order, stopping at the first failure. */
export const run = async (commands: readonly string[], cwd: string, options: RunOptions = {}): Promise<RunResult> => {
  const { capture = false, watchStderr = false } = options;
  let output = "";
  let stderr = "";
  for (const command of commands) {
    const child = Bun.spawn(["bash", "-c", command], {
      cwd,
      stdin: capture ? "ignore" : "inherit",
      stdout: capture ? "pipe" : "inherit",
      stderr: capture || watchStderr ? "pipe" : "inherit",
    });
    if (capture) {
      const [out, err] = await Promise.all([
        new Response(child.stdout as ReadableStream).text(),
        new Response(child.stderr as ReadableStream).text(),
      ]);
      output += out + err;
      stderr = (stderr + err).slice(-STDERR_KEPT);
    } else if (watchStderr) {
      const decoder = new TextDecoder();
      for await (const chunk of child.stderr as ReadableStream<Uint8Array>) {
        process.stderr.write(chunk);
        stderr = (stderr + decoder.decode(chunk, { stream: true })).slice(-STDERR_KEPT);
      }
    }
    const exitCode = await child.exited;
    if (exitCode !== 0) return { exitCode, ...(capture ? { output } : {}), stderr };
  }
  return { exitCode: 0, ...(capture ? { output } : {}), stderr };
};
