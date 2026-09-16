import { describe, it, expect } from "vitest";
import { previewUrl, mediaManifest } from "./MediaReview";
import { reviewSchema } from "./approval-contracts";
describe("isolated preview contract", () => {
  it("rejects same-host, noncanonical and non-HTTPS destinations", () => {
    for (const origin of [
      null,
      "https://console.test",
      "https://console.test:444",
      "http://preview.test",
      "https://preview.test/",
      "https://x@preview.test",
      "https://preview.test/?x=1",
      "bad",
    ]) {
      expect(previewUrl(origin, "https://console.test")).toBeNull();
    }
    expect(previewUrl("https://preview.test", "https://console.test")).toBe(
      "https://preview.test/review.html",
    );
  });
  it("rejects oversized or incomplete media manifests", () => {
    const id = "11111111-1111-4111-8111-111111111111",
      expires = new Date().toISOString();
    const m = {
      contract: "isolated-mp4-review-v1",
      session_id: id,
      approval_id: id,
      expires_at: expires,
      output: {
        contract: "current-turn-output-v1",
        sha256: "a".repeat(64),
        byte_length: 5,
        chunk_count: 1,
        chunk_bytes: 4194304,
        expires_at: expires,
        scope: "current_turn_only",
      },
    };
    expect(mediaManifest.safeParse(m).success).toBe(true);
    expect(
      mediaManifest.safeParse({ ...m, output: { ...m.output, chunk_count: 2 } })
        .success,
    ).toBe(false);
    expect(
      mediaManifest.safeParse({
        ...m,
        output: { ...m.output, byte_length: 67108865 },
      }).success,
    ).toBe(false);
    expect(
      reviewSchema.safeParse({ stage: "media_output", can_continue: true })
        .success,
    ).toBe(false);
  });
});
