import type { Reply } from "./helpers.tsx";

export interface RecordedUpload {
  method: string;
  url: string;
  headers: Record<string, string>;
  /** The body object handed to `send` (a Blob/File: it is not read into memory). */
  body: unknown;
  aborted: boolean;
}

export type UploadHandler = (request: RecordedUpload) => Reply | "hang" | "network-error";

/**
 * A stand-in for XMLHttpRequest that supports exactly what the upload code
 * uses: headers, upload progress, load, error and abort. `hang` keeps the
 * request open until `abort()` so cancellation can be tested.
 */
export function fakeXhr(handler: UploadHandler) {
  const requests: RecordedUpload[] = [];

  class FakeXhr {
    upload: { onprogress: ((event: { loaded: number }) => void) | null } = { onprogress: null };
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    ontimeout: (() => void) | null = null;
    onabort: (() => void) | null = null;
    status = 0;
    statusText = "";
    responseText = "";
    responseType = "";
    withCredentials = false;
    private readonly headers: Record<string, string> = {};
    private responseHeaders: Record<string, string> = {};
    private record: RecordedUpload = { method: "", url: "", headers: {}, body: undefined, aborted: false };
    private open_ = false;

    open(method: string, url: string) {
      this.record = { method, url, headers: this.headers, body: undefined, aborted: false };
      requests.push(this.record);
      this.open_ = true;
    }
    setRequestHeader(name: string, value: string) {
      this.headers[name.toLowerCase()] = value;
    }
    getResponseHeader(name: string) {
      return this.responseHeaders[name.toLowerCase()] ?? null;
    }
    send(body: unknown) {
      this.record.body = body;
      const size = body instanceof Blob ? body.size : 0;
      const outcome = handler(this.record);
      setTimeout(() => {
        if (!this.open_) return;
        this.upload.onprogress?.({ loaded: Math.floor(size / 2) });
        if (outcome === "hang") return;
        this.open_ = false;
        if (outcome === "network-error") {
          this.onerror?.();
          return;
        }
        this.status = outcome.status ?? 200;
        this.responseText = outcome.body === undefined ? "" : JSON.stringify(outcome.body);
        this.responseHeaders = Object.fromEntries(
          Object.entries(outcome.headers ?? {}).map(([k, v]) => [k.toLowerCase(), v]),
        );
        this.onload?.();
      }, 0);
    }
    abort() {
      if (!this.open_) return;
      this.open_ = false;
      this.record.aborted = true;
      this.onabort?.();
    }
  }

  return { factory: () => new FakeXhr() as unknown as XMLHttpRequest, requests };
}
