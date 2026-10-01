import { createClient, type Client, type InValue, type Row } from "@libsql/client";
import { mkdirSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { dirname, join } from "node:path";
import { SEED_SKILLS } from "./seed.ts";
import type { BrainEvent, Path, Skill } from "./types.ts";

/**
 * Where the brain lives. One libSQL client covers every deployment:
 *
 *   (default)                         local SQLite file at ~/.jli/brain.db
 *   JLI_DB_URL=file:/path/brain.db    local SQLite file somewhere else
 *   JLI_DB_URL=libsql://x.turso.io    remote cloud database, shared by all hosts
 *   JLI_SYNC_URL=libsql://x.turso.io  local replica that syncs with the cloud
 *                                     (local-speed reads, shared learning)
 *   JLI_DB_TOKEN=...                  auth token for the remote database
 */
export interface BrainConfig {
  url: string;
  syncUrl?: string;
  authToken?: string;
}

export function brainConfigFromEnv(env: Record<string, string | undefined> = process.env): BrainConfig {
  const url = env.JLI_DB_URL?.trim() || `file:${join(homedir(), ".jli", "brain.db")}`;
  return {
    url,
    syncUrl: env.JLI_SYNC_URL?.trim() || undefined,
    authToken: env.JLI_DB_TOKEN?.trim() || undefined,
  };
}

export const MAX_EXAMPLES = 16;
const HOST = hostname();

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS skills (
     label            TEXT PRIMARY KEY,
     steps            TEXT NOT NULL,
     scenario         TEXT NOT NULL,
     examples         TEXT NOT NULL DEFAULT '[]',
     counter_examples TEXT NOT NULL DEFAULT '[]',
     slots            TEXT NOT NULL DEFAULT '[]',
     risk             TEXT NOT NULL DEFAULT 'safe',
     origin           TEXT NOT NULL DEFAULT 'seed',
     uses             INTEGER NOT NULL DEFAULT 0,
     created_at       TEXT NOT NULL,
     last_used_at     TEXT,
     created_host     TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS events (
     id          INTEGER PRIMARY KEY AUTOINCREMENT,
     ts          TEXT NOT NULL,
     host        TEXT NOT NULL,
     cwd         TEXT NOT NULL,
     query       TEXT NOT NULL,
     path        TEXT NOT NULL,
     label       TEXT,
     confidence  REAL,
     command     TEXT,
     exit_code   INTEGER,
     accepted    INTEGER NOT NULL DEFAULT 0,
     learned     TEXT,
     ms_system1  INTEGER,
     ms_system2  INTEGER
   )`,
  `CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)`,
];

const json = <T>(value: unknown, fallback: T): T => {
  try {
    return JSON.parse(String(value)) as T;
  } catch {
    return fallback;
  }
};

const toSkill = (r: Row): Skill => ({
  label: String(r.label),
  steps: json<string[]>(r.steps, []),
  scenario: String(r.scenario),
  examples: json<string[]>(r.examples, []),
  counterExamples: json<string[]>(r.counter_examples, []),
  slots: json<Skill["slots"]>(r.slots, []),
  risk: String(r.risk) as Skill["risk"],
  origin: String(r.origin) as Skill["origin"],
  uses: Number(r.uses),
  createdAt: String(r.created_at),
  lastUsedAt: r.last_used_at == null ? null : String(r.last_used_at),
  createdHost: String(r.created_host),
});

const toEvent = (r: Row): BrainEvent => ({
  id: Number(r.id),
  ts: String(r.ts),
  host: String(r.host),
  cwd: String(r.cwd),
  query: String(r.query),
  path: String(r.path) as Path,
  label: r.label == null ? null : String(r.label),
  confidence: r.confidence == null ? null : Number(r.confidence),
  command: r.command == null ? null : String(r.command),
  exitCode: r.exit_code == null ? null : Number(r.exit_code),
  accepted: Number(r.accepted) === 1,
  learned: r.learned == null ? null : String(r.learned),
  msSystem1: r.ms_system1 == null ? null : Number(r.ms_system1),
  msSystem2: r.ms_system2 == null ? null : Number(r.ms_system2),
});

export type NewSkill = Pick<Skill, "label" | "steps" | "scenario" | "risk" | "origin"> &
  Partial<Pick<Skill, "examples" | "slots">>;

export type NewEvent = Omit<BrainEvent, "id" | "ts" | "host">;

/** The persistent decision matrix plus the log it learns from. */
export class Brain {
  readonly #db: Client;
  readonly config: BrainConfig;

  private constructor(db: Client, config: BrainConfig) {
    this.#db = db;
    this.config = config;
  }

  static async open(config: BrainConfig = brainConfigFromEnv()): Promise<Brain> {
    if (config.url.startsWith("file:")) {
      mkdirSync(dirname(config.url.slice("file:".length)), { recursive: true });
    }
    const db = createClient({ url: config.url, syncUrl: config.syncUrl, authToken: config.authToken });
    const brain = new Brain(db, config);
    if (config.syncUrl) await brain.sync();
    await db.batch(SCHEMA, "write");
    await brain.#seedOnce();
    return brain;
  }

  /** Where this brain is stored, for display. */
  get location(): string {
    const { url, syncUrl } = this.config;
    if (syncUrl) return `${url} ⇄ ${syncUrl}`;
    return url;
  }

  get isShared(): boolean {
    return Boolean(this.config.syncUrl) || !this.config.url.startsWith("file:");
  }

  async #seedOnce(): Promise<void> {
    const seeded = await this.#db.execute("SELECT value FROM meta WHERE key = 'seeded'");
    if (seeded.rows.length > 0) return;
    for (const skill of SEED_SKILLS) await this.addSkill(skill, { ifMissing: true });
    await this.#db.execute({
      sql: "INSERT OR IGNORE INTO meta (key, value) VALUES ('seeded', ?)",
      args: [new Date().toISOString()],
    });
  }

  /** Pull and push the local replica. A no-op without `JLI_SYNC_URL`. */
  async sync(): Promise<boolean> {
    if (!this.config.syncUrl) return false;
    await this.#db.sync();
    return true;
  }

  close(): void {
    this.#db.close();
  }

  async skills(): Promise<Skill[]> {
    const { rows } = await this.#db.execute("SELECT * FROM skills ORDER BY uses DESC, label ASC");
    return rows.map(toSkill);
  }

  async skill(label: string): Promise<Skill | undefined> {
    const { rows } = await this.#db.execute({ sql: "SELECT * FROM skills WHERE label = ?", args: [label] });
    const row = rows[0];
    return row ? toSkill(row) : undefined;
  }

  async addSkill(skill: NewSkill, options: { ifMissing?: boolean } = {}): Promise<void> {
    // Re-teaching a label changes what it runs but keeps what was learned about it.
    const conflict = options.ifMissing
      ? "DO NOTHING"
      : "DO UPDATE SET steps = excluded.steps, scenario = excluded.scenario, slots = excluded.slots, risk = excluded.risk";
    await this.#db.execute({
      sql: `INSERT INTO skills (label, steps, scenario, examples, slots, risk, origin, created_at, created_host)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(label) ${conflict}`,
      args: [
        skill.label,
        JSON.stringify(skill.steps),
        skill.scenario,
        JSON.stringify(skill.examples ?? []),
        JSON.stringify(skill.slots ?? []),
        skill.risk,
        skill.origin,
        new Date().toISOString(),
        HOST,
      ],
    });
  }

  /**
   * Link a phrase to a command. If a skill already runs exactly this command the
   * phrase is added to it, so one command can collect many phrasings.
   */
  async learnCommand(phrase: string, steps: string[], risk: Skill["risk"]): Promise<{ label: string; created: boolean }> {
    const skills = await this.skills();
    const wanted = steps.join(" && ");
    const existing = skills.find((skill) => skill.steps.join(" && ") === wanted);
    if (existing) {
      await this.linkPhrase(existing.label, phrase);
      return { label: existing.label, created: false };
    }
    let label = wanted.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 48) || "skill";
    while (skills.some((skill) => skill.label === label)) label = `${label}-x`;
    await this.addSkill({ label, steps, scenario: `the user wants: ${phrase}`, risk, origin: "taught", examples: [phrase] });
    return { label, created: true };
  }

  /** Add a phrasing to a skill, and drop it from that skill's counter-examples if it was one. */
  async linkPhrase(label: string, phrase: string): Promise<void> {
    await this.addExample(label, phrase);
    await this.#editList(label, "counter_examples", (list) => {
      const next = list.filter((e) => !same(e, phrase));
      return next.length === list.length ? undefined : next;
    });
  }

  async setting(key: string): Promise<string | undefined> {
    const { rows } = await this.#db.execute({ sql: "SELECT value FROM meta WHERE key = ?", args: [`setting:${key}`] });
    const row = rows[0];
    return row ? String(row.value) : undefined;
  }

  async setSetting(key: string, value: string): Promise<void> {
    await this.#db.execute({
      sql: "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      args: [`setting:${key}`, value],
    });
  }

  async forget(label: string): Promise<boolean> {
    const result = await this.#db.execute({ sql: "DELETE FROM skills WHERE label = ?", args: [label] });
    return result.rowsAffected > 0;
  }

  /** Remember that `phrase` means `label`. Returns false if it was already known. */
  async addExample(label: string, phrase: string): Promise<boolean> {
    return this.#editList(label, "examples", (list) => {
      if (list.some((e) => same(e, phrase))) return undefined;
      return [...list, phrase].slice(-MAX_EXAMPLES);
    });
  }

  async removeExample(label: string, phrase: string): Promise<boolean> {
    return this.#editList(label, "examples", (list) => {
      const next = list.filter((e) => !same(e, phrase));
      return next.length === list.length ? undefined : next;
    });
  }

  /** Remember that `phrase` does NOT mean `label`. */
  async addCounterExample(label: string, phrase: string): Promise<boolean> {
    return this.#editList(label, "counter_examples", (list) => {
      if (list.some((e) => same(e, phrase))) return undefined;
      return [...list, phrase].slice(-MAX_EXAMPLES);
    });
  }

  async #editList(
    label: string,
    column: "examples" | "counter_examples",
    edit: (list: string[]) => string[] | undefined,
  ): Promise<boolean> {
    const { rows } = await this.#db.execute({ sql: `SELECT ${column} AS list FROM skills WHERE label = ?`, args: [label] });
    const row = rows[0];
    if (!row) return false;
    const next = edit(json<string[]>(row.list, []));
    if (!next) return false;
    await this.#db.execute({
      sql: `UPDATE skills SET ${column} = ? WHERE label = ?`,
      args: [JSON.stringify(next), label],
    });
    return true;
  }

  async recordUse(label: string): Promise<void> {
    await this.#db.execute({
      sql: "UPDATE skills SET uses = uses + 1, last_used_at = ? WHERE label = ?",
      args: [new Date().toISOString(), label],
    });
  }

  async logEvent(event: NewEvent): Promise<number> {
    const args: InValue[] = [
      new Date().toISOString(),
      HOST,
      event.cwd,
      event.query,
      event.path,
      event.label,
      event.confidence,
      event.command,
      event.exitCode,
      event.accepted ? 1 : 0,
      event.learned,
      event.msSystem1,
      event.msSystem2,
    ];
    const result = await this.#db.execute({
      sql: `INSERT INTO events (ts, host, cwd, query, path, label, confidence, command, exit_code, accepted, learned, ms_system1, ms_system2)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      args,
    });
    return Number(result.lastInsertRowid ?? 0);
  }

  async markRejected(eventId: number): Promise<void> {
    await this.#db.execute({ sql: "UPDATE events SET accepted = 0, learned = 'corrected' WHERE id = ?", args: [eventId] });
  }

  /** Newest first. */
  async events(limit = 20): Promise<BrainEvent[]> {
    const { rows } = await this.#db.execute({ sql: "SELECT * FROM events ORDER BY id DESC LIMIT ?", args: [limit] });
    return rows.map(toEvent);
  }

  async lastExecuted(): Promise<BrainEvent | undefined> {
    const { rows } = await this.#db.execute(
      "SELECT * FROM events WHERE label IS NOT NULL AND accepted = 1 ORDER BY id DESC LIMIT 1",
    );
    const row = rows[0];
    return row ? toEvent(row) : undefined;
  }
}

const same = (a: string, b: string) => a.trim().toLowerCase() === b.trim().toLowerCase();
