// The gateway's stderr: all of it to a file, the end of it to the screen.
//
// Two jobs and one reason. Everything goes to `<instance>/gateway.log`, which is
// what there is to read after the fact. The last few lines stay in memory, because
// the launch screen's whole value is showing the gateway's own complaint — a config
// error names the target and the key, and no sentence this app could write in front
// of it would be as useful.

/** How much of the tail the launch screen gets. */
export const TAIL_LINES = 60;

export class LogTail {
  private lines: string[] = [];
  private partial = "";

  /**
   * Take a chunk of stderr as it arrives.
   *
   * Chunks are not lines: a write can split one and two can arrive together, so
   * the remainder is carried until its newline shows up.
   */
  push(chunk: string): void {
    const text = this.partial + chunk;
    const parts = text.split("\n");
    this.partial = parts.pop() ?? "";
    for (const line of parts) {
      this.lines.push(line);
    }
    if (this.lines.length > TAIL_LINES) {
      this.lines = this.lines.slice(-TAIL_LINES);
    }
  }

  /**
   * What the process said, ending with whatever it had not finished saying.
   *
   * The unterminated remainder is included deliberately: a program dying part way
   * through its last sentence is exactly the case this is read in, and dropping
   * that line would drop the one that matters.
   */
  text(): string {
    const all =
      this.partial === "" ? this.lines : [...this.lines, this.partial];
    return all.slice(-TAIL_LINES).join("\n");
  }

  /** True when the process has said nothing at all — a different failure. */
  isEmpty(): boolean {
    return this.lines.length === 0 && this.partial === "";
  }

  clear(): void {
    this.lines = [];
    this.partial = "";
  }
}
