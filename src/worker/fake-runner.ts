// A fake session runner for tests: sessions hand out ids, record the prompts they are sent, and
// hold each turn open until the test releases it, so queueing and interruption can be exercised.

import type { Runner, Session, SessionOptions, TurnResult } from "../session/runner.ts";

export class FakeSession implements Session {
  readonly prompts: string[] = [];
  /** Turns that have started and not finished; the test calls `release` to finish the oldest. */
  readonly #open: { resolve: (r: TurnResult) => void; reject: (e: Error) => void }[] = [];
  /** When true a turn finishes at once instead of waiting for `release`. */
  auto = true;
  failWith: Error | undefined;
  running = 0;
  maxRunning = 0;

  readonly id: string;
  readonly options: SessionOptions;
  readonly resumed: boolean;

  constructor(id: string, options: SessionOptions, resumed: boolean) {
    this.id = id;
    this.options = options;
    this.resumed = resumed;
  }

  async send(prompt: string): Promise<TurnResult> {
    this.prompts.push(prompt);
    this.running++;
    this.maxRunning = Math.max(this.maxRunning, this.running);
    try {
      if (this.failWith !== undefined) throw this.failWith;
      const result: TurnResult = {
        sessionId: this.id,
        text: "ok",
        status: "completed",
        usage: { inputTokens: 10, cachedInputTokens: 5, outputTokens: 2 },
        model: null,
      };
      if (this.auto) return result;
      return await new Promise<TurnResult>((resolve, reject) => this.#open.push({ resolve, reject }));
    } finally {
      this.running--;
    }
  }

  release(): void {
    this.#open.shift()!.resolve({
      sessionId: this.id,
      text: "ok",
      status: "completed",
      usage: { inputTokens: 10, cachedInputTokens: 5, outputTokens: 2 },
      model: null,
    });
  }
}

export class FakeRunner implements Runner {
  readonly sessions: FakeSession[] = [];
  /** Set to make new sessions hold their turns until released. */
  hold = false;
  /** Set to make the next started session fail before it reports an id. */
  startFailure: Error | undefined;
  /** Set to make new sessions report their id and then fail the turn. */
  failAfterId: Error | undefined;

  start(options: SessionOptions): Session {
    const session = new FakeSession(`sess-${this.sessions.length + 1}`, options, false);
    session.auto = !this.hold;
    this.sessions.push(session);
    if (this.startFailure !== undefined) {
      session.failWith = this.startFailure;
      return session;
    }
    const send = session.send.bind(session);
    session.send = async (prompt) => {
      options.onSessionId?.(session.id);
      if (this.failAfterId !== undefined) throw this.failAfterId;
      return send(prompt);
    };
    return session;
  }

  resume(options: SessionOptions & { sessionId: string }): Session {
    const session = new FakeSession(options.sessionId, options, true);
    session.auto = !this.hold;
    this.sessions.push(session);
    return session;
  }
}
