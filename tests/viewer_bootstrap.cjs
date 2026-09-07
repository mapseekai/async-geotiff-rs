// Run with: node tests/viewer_bootstrap.cjs
// Execute the actual inline viewers with delayed metadata and lightweight map stubs.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

async function check(viewer) {
  const html = fs.readFileSync(`examples/${viewer}.html`, "utf8");
  const requests = [], pending = [], maps = [], elements = new Map();
  class Element {
    constructor(name = "", value = "") { this.name = name; this.value = value; this.options = []; this.events = {}; }
    addEventListener(name, fn) { (this.events[name] ??= []).push(fn); }
    requestSubmit() { for (const fn of this.events.submit ?? []) fn({preventDefault() {}}); }
    appendChild(option) { this.options.push(option); if (!this.value) this.value = option.value; }
    add(option) { this.appendChild(option); }
    set innerHTML(_) { this.options = []; this.value = ""; }
  }
  for (const match of html.matchAll(/<(input|select|form|button|label|small|div)[^>]*\bid="([^"]+)"[^>]*>/g)) {
    const tag = match[0], name = tag.match(/\bname="([^"]*)"/)?.[1] ?? match[2];
    const element = new Element(name, tag.match(/\bvalue="([^"]*)"/)?.[1] ?? "");
    if (match[1] === "select") {
      const body = html.slice(match.index + tag.length, html.indexOf("</select>", match.index));
      element.options = [...body.matchAll(/<option[^>]*value="([^"]*)"/g)].map(m => ({value:m[1]}));
      element.value = element.options[0]?.value ?? "";
    }
    elements.set(match[2], element);
  }
  const record = url => { if (url?.includes("/tiles/")) requests.push(url); };
  class FakeMap {
    constructor(options = {}) {
      this.events = {}; this.loaded = false; this.sources = {...options.style?.sources}; this.layers = {};
      for (const source of Object.values(this.sources)) for (const url of source.tiles ?? []) record(url);
      maps.push(this);
    }
    on(event, fn) { (this.events[event] ??= []).push(fn); }
    once(event, fn) { this.on(event, fn); }
    emitLoad() { this.loaded = true; for (const fn of this.events.load ?? []) fn(); }
    isStyleLoaded() { return this.loaded; }
    addSource(id, source) { this.sources[id] = source; for (const url of source.tiles ?? []) record(url); }
    addLayer(layer) { this.layers[layer.id] = layer; }
    getLayer(id) { return this.layers[id]; }
    getSource(id) { return this.sources[id]; }
    removeLayer(id) { delete this.layers[id]; }
    removeSource(id) { delete this.sources[id]; }
    getCenter() { return {lng:0,lat:0}; }
    getZoom() { return 2; }
    getSize() { return [256,256]; }
    getView() { return {fit() {}}; }
    fitBounds() {} addControl() {} setPaintProperty() {}
  }
  class Layer { setSource() {} setOpacity() {} }
  const context = {
    console, URLSearchParams, Number, Boolean,
    window:{location:{search:"?dataset=single.tif&bidx=1&bidx=2&bidx=3", protocol:"http:", origin:"http://test"}, history:{replaceState() {}}},
    document:{getElementById:id => elements.get(id), createElement:() => new Element()},
    Option: function(label, value) { this.value = value; this.textContent = label; },
    fetch:url => new Promise(resolve => pending.push({url,resolve})),
    maplibregl:{Map:FakeMap,NavigationControl:class {},ScaleControl:class {}},
    L:{map:() => new FakeMap(),tileLayer:url => { record(url); return {addTo(){return this;},bringToBack(){},setOpacity(){}}; }},
    ol:{source:{XYZ:class {constructor({url}) {record(url);}},OSM:class {}},layer:{Tile:Layer},Map:FakeMap,View:class {},proj:{transformExtent:b=>b}},
  };
  vm.createContext(context);
  for (const match of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)) vm.runInContext(match[1], context);
  const flush = async () => { for (let i=0;i<10;i++) await Promise.resolve(); };
  const reply = (part, data, bands) => {
    const call = pending.find(p=>p.url.includes(part)); assert.ok(call, `${viewer}: missing ${part}`);
    call.resolve({ok:true,json:async()=>data,headers:{get:()=>bands ?? null}});
  };
  assert.equal(requests.length,0,`${viewer}: raster fetched during bootstrap`);
  maps.forEach(map=>map.emitLoad());
  reply("/formats",[{value:"png",label:"PNG"}]); await flush();
  assert.equal(requests.length,0,`${viewer}: format readiness started raster before dataset`);
  reply("/datasets",["single.tif"]); await flush();
  assert.equal(requests.length,0,`${viewer}: dataset list started raster before metadata`);
  reply("/bounds",[0,0,1,1],"1"); await flush();
  assert.ok(requests.length > 0,`${viewer}: raster never started`);
  for (const url of requests) {
    const params = new URL(url).searchParams;
    assert.equal(params.get("dataset"),"single.tif");
    assert.equal(params.has("bidx"),false,`${viewer}: legacy RGB preset survived single-band metadata`);
  }
  console.log(`${viewer}: delayed dataset/metadata bootstrap passed`);
}
(async()=>{for(const viewer of ["maplibre","leaflet","openlayers"]) await check(viewer);})().catch(error=>{console.error(error);process.exitCode=1;});
