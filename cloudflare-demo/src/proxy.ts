export interface DemoContainerHandle {
  startAndWaitForPorts(): Promise<void>;
  fetch(request: Request): Promise<Response>;
}

export function demoUnavailableResponse(): Response {
  return new Response("Demo temporarily unavailable. Please retry shortly.", {
    status: 503,
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "retry-after": "10",
    },
  });
}

export async function proxyDemoRequest(
  request: Request,
  container: DemoContainerHandle,
): Promise<Response> {
  try {
    await container.startAndWaitForPorts();
    return await container.fetch(request);
  } catch (error) {
    console.error("demo container unavailable", error);
    return demoUnavailableResponse();
  }
}
