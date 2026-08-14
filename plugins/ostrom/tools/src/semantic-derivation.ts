import { createHash } from "node:crypto";

export const semanticFindingKinds = [
  "parked",
  "already_decided",
  "genuinely_stuck",
  "actually_a_release",
] as const;

export type SemanticFindingKind = (typeof semanticFindingKinds)[number];
export type EvidenceSource = "title" | "label" | "body" | "comment";
export type MechanicalClassification =
  | "delegated"
  | "excluded"
  | "unclassified"
  | "reserved"
  | "tripwire";
export type AdvisoryClassification = "unclassified" | "reserved" | "tripwire";

export interface SemanticContent {
  title: string;
  labels: string[];
  body: string;
  comments: string[];
}

export interface MechanicalVerdict {
  classification: MechanicalClassification;
  matched_selector: string;
  matched_source: string;
}

export interface QuotedEvidence {
  source: EvidenceSource;
  quote: string;
}

export interface SemanticFinding {
  kind: SemanticFindingKind;
  confidence: number;
  evidence: QuotedEvidence;
}

export interface AuthorityEscalation {
  classification: AdvisoryClassification;
  confidence: number;
  evidence: QuotedEvidence;
}

export interface RawSemanticVerdict {
  findings: SemanticFinding[];
  authority?: {
    classification: string;
    confidence: number;
    evidence: QuotedEvidence;
  } | null;
}

export interface SemanticDerivation {
  findings: SemanticFinding[];
  authority: AuthorityEscalation | null;
}

export class RejectedSemanticVerdict extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RejectedSemanticVerdict";
  }
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function exactKeys(
  value: Record<string, unknown>,
  required: string[],
  optional: string[] = [],
): boolean {
  const keys = Object.keys(value);
  return required.every((key) => keys.includes(key)) &&
    keys.every((key) => required.includes(key) || optional.includes(key));
}

function validateEvidence(
  value: unknown,
  content: SemanticContent,
): QuotedEvidence {
  if (!isPlainObject(value) || !exactKeys(value, ["source", "quote"])) {
    throw new RejectedSemanticVerdict("evidence must contain only source and quote");
  }
  const source = value.source;
  const quote = value.quote;
  if (
    typeof source !== "string" ||
    !(["title", "label", "body", "comment"] as string[]).includes(source) ||
    typeof quote !== "string" ||
    quote.length === 0
  ) {
    throw new RejectedSemanticVerdict("evidence source and quote are invalid");
  }

  const copiedVerbatim =
    source === "title"
      ? content.title.includes(quote)
      : source === "label"
        ? content.labels.includes(quote)
        : source === "body"
          ? content.body.includes(quote)
          : content.comments.some((comment) => comment.includes(quote));
  if (!copiedVerbatim) {
    throw new RejectedSemanticVerdict(
      "evidence quote was not copied verbatim from the named source",
    );
  }
  return { source: source as EvidenceSource, quote };
}

function validateConfidence(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0 || value > 1) {
    throw new RejectedSemanticVerdict("confidence must be a number from 0 through 1");
  }
  return value;
}

function validateFinding(value: unknown, content: SemanticContent): SemanticFinding {
  if (!isPlainObject(value) || !exactKeys(value, ["kind", "confidence", "evidence"])) {
    throw new RejectedSemanticVerdict(
      "each finding must contain only kind, confidence, and evidence",
    );
  }
  if (
    typeof value.kind !== "string" ||
    !(semanticFindingKinds as readonly string[]).includes(value.kind)
  ) {
    throw new RejectedSemanticVerdict("finding kind is not in the derivation contract");
  }
  return {
    kind: value.kind as SemanticFindingKind,
    confidence: validateConfidence(value.confidence),
    evidence: validateEvidence(value.evidence, content),
  };
}

function validateAuthority(
  value: unknown,
  content: SemanticContent,
  mechanical: MechanicalVerdict,
): AuthorityEscalation | null {
  if (value === undefined || value === null) return null;
  if (
    !isPlainObject(value) ||
    !exactKeys(value, ["classification", "confidence", "evidence"]) ||
    typeof value.classification !== "string"
  ) {
    throw new RejectedSemanticVerdict(
      "authority must contain only classification, confidence, and evidence",
    );
  }

  // This is the authorization boundary. The prompt is intentionally not
  // trusted: public issue text can instruct the model to delegate. A model
  // may name only a judgment-requiring classification, and an existing
  // reserved ref or bounce/tripwire match may not be cleared or replaced.
  // Mechanical classification itself is never mutated by this module.
  if (value.classification === "delegated" || value.classification === "excluded") {
    throw new RejectedSemanticVerdict(
      `model authority ${value.classification} would widen or bypass mechanical authority`,
    );
  }
  if (!(["unclassified", "reserved", "tripwire"] as string[]).includes(value.classification)) {
    throw new RejectedSemanticVerdict("model authority classification is unknown");
  }
  if (
    (mechanical.classification === "reserved" && value.classification !== "reserved") ||
    (mechanical.classification === "tripwire" && value.classification !== "tripwire")
  ) {
    throw new RejectedSemanticVerdict(
      `model authority ${value.classification} would clear mechanical ${mechanical.classification}`,
    );
  }

  return {
    classification: value.classification as AdvisoryClassification,
    confidence: validateConfidence(value.confidence),
    evidence: validateEvidence(value.evidence, content),
  };
}

export function validateSemanticVerdict(
  value: unknown,
  content: SemanticContent,
  mechanical: MechanicalVerdict,
): SemanticDerivation {
  if (!isPlainObject(value) || !exactKeys(value, ["findings"], ["authority"])) {
    throw new RejectedSemanticVerdict(
      "verdict must contain findings and optional authority only",
    );
  }
  if (!Array.isArray(value.findings)) {
    throw new RejectedSemanticVerdict("findings must be an array");
  }
  const findings = value.findings.map((finding) => validateFinding(finding, content));
  if (new Set(findings.map((finding) => finding.kind)).size !== findings.length) {
    throw new RejectedSemanticVerdict("a verdict may contain at most one finding of each kind");
  }
  return {
    findings,
    authority: validateAuthority(value.authority, content, mechanical),
  };
}

export function semanticContentHash(content: SemanticContent): string {
  // Explicit field order makes the cache key portable across ports and runs.
  const canonical = JSON.stringify({
    title: content.title,
    labels: content.labels,
    body: content.body,
    comments: content.comments,
  });
  return createHash("sha256").update(canonical).digest("hex");
}
