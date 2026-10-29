// @vitest-environment node
import { createHash, randomBytes } from "node:crypto";
import { describe, expect, it } from "vitest";
import { HASH_CHUNK_BYTES, Sha256, sha256OfBlob } from "./sha256.ts";

const node = (data: Uint8Array) => createHash("sha256").update(data).digest("hex");

describe("incremental SHA-256", () => {
  it("matches the published test vectors", () => {
    const enc = new TextEncoder();
    expect(new Sha256().hex()).toBe("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    expect(new Sha256().update(enc.encode("abc")).hex()).toBe(
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );
    expect(
      new Sha256().update(enc.encode("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")).hex(),
    ).toBe("248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
  });

  it("agrees with node:crypto for every length around the block and padding boundaries", () => {
    for (let length = 0; length <= 200; length += 1) {
      const data = randomBytes(length);
      expect(new Sha256().update(data).hex(), `length ${length}`).toBe(node(data));
    }
  });

  it("gives the same digest however the input is split", () => {
    const data = randomBytes(10_000);
    const hash = new Sha256();
    for (let offset = 0, step = 1; offset < data.length; offset += step, step += 7) {
      hash.update(data.subarray(offset, offset + step));
    }
    expect(hash.hex()).toBe(node(data));
  });

  it("refuses updates after the digest", () => {
    const hash = new Sha256();
    hash.hex();
    expect(() => hash.update(new Uint8Array(1))).toThrow();
  });

  it("hashes a blob in chunks (more than one chunk) and reports progress", async () => {
    const data = randomBytes(HASH_CHUNK_BYTES * 2 + 123);
    const seen: number[] = [];
    const digest = await sha256OfBlob(new Blob([data]), { onProgress: (done) => seen.push(done) });
    expect(digest).toBe(node(data));
    expect(seen).toEqual([HASH_CHUNK_BYTES, HASH_CHUNK_BYTES * 2, data.length]);
  });

  it("hashes an empty blob and honours an aborted signal", async () => {
    expect(await sha256OfBlob(new Blob([]))).toBe(node(new Uint8Array()));
    const controller = new AbortController();
    controller.abort();
    await expect(sha256OfBlob(new Blob([randomBytes(10)]), { signal: controller.signal })).rejects.toMatchObject({
      name: "AbortError",
    });
  });
});
