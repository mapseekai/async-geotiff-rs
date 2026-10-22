const test = require("node:test");
const checkViewer = require("../tests/support/viewer.cjs");

test("openlayers viewer", () => checkViewer("openlayers"));
