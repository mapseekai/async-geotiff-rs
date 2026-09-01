import { expect, it, vi } from "vitest";
import { proxyDemoRequest } from "./proxy";

it("starts the demo before forwarding the request", async () => {
  const container = {
    startAndWaitForPorts: vi.fn().mockResolvedValue(undefined),
    fetch: vi.fn().mockResolvedValue(new Response("ok")),
  };
  const request = new Request("https://demo.example/maplibre");

  const response = await proxyDemoRequest(request, container);

  expect(container.startAndWaitForPorts).toHaveBeenCalledOnce();
  expect(container.fetch).toHaveBeenCalledOnce();
  expect(container.fetch).toHaveBeenCalledWith(request);
  expect(response.status).toBe(200);
  expect(await response.text()).toBe("ok");
});

it("returns a retry-safe response when the demo cannot start", async () => {
  const container = {
    startAndWaitForPorts: vi.fn().mockRejectedValue(new Error("unavailable")),
    fetch: vi.fn(),
  };

  const response = await proxyDemoRequest(
    new Request("https://demo.example/maplibre"),
    container,
  );

  expect(response.status).toBe(503);
  expect(response.headers.get("retry-after")).toBe("10");
  expect(await response.text()).toBe(
    "Demo temporarily unavailable. Please retry shortly.",
  );
  expect(container.fetch).not.toHaveBeenCalled();
});

it("returns a retry-safe response when forwarding to the demo fails", async () => {
  const container = {
    startAndWaitForPorts: vi.fn().mockResolvedValue(undefined),
    fetch: vi.fn().mockRejectedValue(new Error("unavailable")),
  };

  const response = await proxyDemoRequest(
    new Request("https://demo.example/maplibre"),
    container,
  );

  expect(container.startAndWaitForPorts).toHaveBeenCalledOnce();
  expect(container.fetch).toHaveBeenCalledOnce();
  expect(response.status).toBe(503);
  expect(response.headers.get("retry-after")).toBe("10");
  expect(await response.text()).toBe(
    "Demo temporarily unavailable. Please retry shortly.",
  );
});
