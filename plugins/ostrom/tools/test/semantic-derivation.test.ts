import { describe, expect, it } from "vitest";
import {
  RejectedSemanticVerdict,
  semanticContentHash,
  validateSemanticVerdict,
  type MechanicalVerdict,
  type SemanticContent,
} from "../src/semantic-derivation.js";

const content: SemanticContent = {
  title: "spec: defer the release",
  labels: ["status:parked"],
  body: "The principal decided to wait until October.",
  comments: ["Parked by agreement on 2026-08-13."],
};

function mechanical(classification: MechanicalVerdict["classification"]): MechanicalVerdict {
  return {
    classification,
    matched_selector: `default:${classification}`,
    matched_source: "default",
  };
}

describe("semantic derivation safety boundary", () => {
  it.each(["reserved", "tripwire"] as const)(
    "rejects a verdict that clears a mechanical %s",
    (classification) => {
      expect(() =>
        validateSemanticVerdict(
          {
            findings: [],
            authority: {
              classification: "unclassified",
              confidence: 0.99,
              evidence: { source: "title", quote: "release" },
            },
          },
          content,
          mechanical(classification),
        ),
      ).toThrow(RejectedSemanticVerdict);
    },
  );

  it("rejects a verdict that tries to clear a bounce match", () => {
    const bounce: MechanicalVerdict = {
      classification: "tripwire",
      matched_selector: "title:*credential*",
      matched_source: "bounce_all",
    };
    expect(() =>
      validateSemanticVerdict(
        {
          findings: [],
          authority: {
            classification: "unclassified",
            confidence: 1,
            evidence: { source: "title", quote: "release" },
          },
        },
        content,
        bounce,
      ),
    ).toThrow(/clear mechanical tripwire/);
  });

  it.each(["delegated", "excluded"])(
    "rejects the widening authority classification %s",
    (classification) => {
      expect(() =>
        validateSemanticVerdict(
          {
            findings: [],
            authority: {
              classification,
              confidence: 1,
              evidence: { source: "body", quote: "principal decided" },
            },
          },
          content,
          mechanical("unclassified"),
        ),
      ).toThrow(/widen or bypass/);
    },
  );

  it("accepts cautious findings with verbatim evidence", () => {
    expect(
      validateSemanticVerdict(
        {
          findings: [
            {
              kind: "parked",
              confidence: 1,
              evidence: { source: "label", quote: "status:parked" },
            },
            {
              kind: "already_decided",
              confidence: 0.9,
              evidence: { source: "body", quote: "principal decided" },
            },
          ],
          authority: null,
        },
        content,
        mechanical("delegated"),
      ),
    ).toEqual({
      findings: [
        {
          kind: "parked",
          confidence: 1,
          evidence: { source: "label", quote: "status:parked" },
        },
        {
          kind: "already_decided",
          confidence: 0.9,
          evidence: { source: "body", quote: "principal decided" },
        },
      ],
      authority: null,
    });
  });

  it("rejects evidence that is not a quoted source span", () => {
    expect(() =>
      validateSemanticVerdict(
        {
          findings: [
            {
              kind: "parked",
              confidence: 0.8,
              evidence: { source: "comment", quote: "made-up summary" },
            },
          ],
        },
        content,
        mechanical("delegated"),
      ),
    ).toThrow(/verbatim/);
  });

  it("keys the cache only on source content", () => {
    expect(semanticContentHash(content)).toBe(semanticContentHash({ ...content }));
    expect(semanticContentHash(content)).not.toBe(
      semanticContentHash({ ...content, body: `${content.body} More text.` }),
    );
  });
});
