// @vitest-environment node
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { ENDPOINTS } from "./client.ts";

// Reads contracts/openapi.yaml without a YAML library: path keys sit at two
// spaces of indentation under `paths:`, operations at four.
function contractOperations(): Map<string, Set<string>> {
  const url = new URL("../../../../contracts/openapi.yaml", import.meta.url);
  const operations = new Map<string, Set<string>>();
  let inPaths = false;
  let current: Set<string> | null = null;
  for (const line of readFileSync(url, "utf8").split(/\r?\n/)) {
    if (/^paths:\s*$/.test(line)) {
      inPaths = true;
      continue;
    }
    if (inPaths && /^\S/.test(line)) {
      break; // next top-level key (components:)
    }
    const path = /^ {2}(\/\S*):\s*$/.exec(line);
    if (inPaths && path?.[1] !== undefined) {
      current = new Set();
      operations.set(path[1], current);
      continue;
    }
    const method = /^ {4}(get|put|post|delete|patch|head|options):\s*$/.exec(line);
    if (inPaths && method?.[1] !== undefined) {
      current?.add(method[1].toUpperCase());
    }
  }
  return operations;
}

describe("OpenAPI contract", () => {
  const operations = contractOperations();

  it("is readable", () => {
    expect(operations.size).toBeGreaterThan(20);
    expect(operations.get("/auth/login")).toEqual(new Set(["POST"]));
  });

  it.each(Object.entries(ENDPOINTS))("%s exists in contracts/openapi.yaml", (_name, endpoint) => {
    const methods = operations.get(endpoint.path);
    expect(methods, `path ${endpoint.path} missing from the contract`).toBeDefined();
    expect(methods?.has(endpoint.method), `${endpoint.method} ${endpoint.path} missing`).toBe(true);
  });
});
