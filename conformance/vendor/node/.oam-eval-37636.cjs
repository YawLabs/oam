console.log(eval("const assert = require('assert');const interval = require('timers/promises').setInterval(1000, null, { ref: false });interval[Symbol.asyncIterator]().next().then(assert.fail)"));
