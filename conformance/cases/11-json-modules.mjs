// JSON modules: default import with the attribute (Node requires it).
import config from "./fixtures/config.json" with { type: "json" };

console.log(config.name, config.port, config.tags.length, config.tags[1]);
console.log(JSON.stringify(config.nested));
