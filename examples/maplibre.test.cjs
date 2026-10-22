const test = require("node:test");
const checkViewer = require("../tests/support/viewer.cjs");

test("maplibre viewer", () => checkViewer("maplibre"));
