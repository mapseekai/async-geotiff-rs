import { Container } from "@cloudflare/containers";
import { proxyDemoRequest } from "./proxy";

const DEMO_COG_URL =
  "https://raw.githubusercontent.com/cogeotiff/rio-tiler/0b08b7f35a8b639cee2f35a0cb565034f7b55bfd/tests/fixtures/cog.tif";

export class DemoContainer extends Container {
  defaultPort = 8080;
  requiredPorts = [8080];
  sleepAfter = "10m";
  enableInternet = true;
  pingEndpoint = "/formats";
  envVars = {
    ASYNC_GEOTIFF_BIND: "0.0.0.0:8080",
    DEMO_COG_URL,
  };
}

export interface Env {
  DEMO_CONTAINER: {
    getByName(name: string): DemoContainer;
  };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const container = env.DEMO_CONTAINER.getByName("demo");
    return proxyDemoRequest(request, container);
  },
};
