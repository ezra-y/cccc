import { afterEach, describe, expect, it, vi } from "vite-plus/test";

import { createWebModelConnectorBinding } from "./webModel";

describe("web model connector binding API", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("requests a one-time binding code for a connector", async () => {
    vi.stubGlobal("window", { location: { search: "" } });
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue(
        new Response(
          JSON.stringify({
            ok: true,
            result: {
              code: "bind_123456",
              binding_expires_at: "2026-09-05T12:00:00Z",
              group_id: "g1",
              actor_id: "a1",
              session_bound: false,
            },
          }),
        ),
      );

    const resp = await createWebModelConnectorBinding("wmc abc/1");

    const [url, init] = fetchMock.mock.calls[0] || [];
    expect(String(url)).toBe("/api/v1/web-model/connectors/wmc%20abc%2F1/binding");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({});
    expect(resp.ok).toBe(true);
    if (resp.ok) {
      expect(resp.result.code).toBe("bind_123456");
      expect(resp.result.binding_expires_at).toBe("2026-09-05T12:00:00Z");
      expect(resp.result.group_id).toBe("g1");
      expect(resp.result.actor_id).toBe("a1");
      expect(resp.result.session_bound).toBe(false);
    }
  });

  it("surfaces API errors to the caller without inventing a code", async () => {
    vi.stubGlobal("window", { location: { search: "" } });
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue(
        new Response(
          JSON.stringify({
            ok: false,
            error: { code: "not_found", message: "web-model connector not found" },
          }),
          { status: 404 },
        ),
      );

    const resp = await createWebModelConnectorBinding("wmc_missing");

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(resp.ok).toBe(false);
    if (!resp.ok) {
      expect(resp.error.message).toBe("web-model connector not found");
      expect(JSON.stringify(resp)).not.toContain("bind_");
    }
  });
});
