// import(): node's ES module loader, as a chain of promise jobs (#397).
//
// node runs import() through its loader (lib/internal/modules/esm/loader.js
// and module_job.js in v26.13.1): inside the call it resolves the specifier,
// loads and translates the module and links it -- each module's requests are
// resolved, loaded and linked synchronously too (ModuleJobBase.syncLink), so
// the whole graph is loaded before import() returns -- and everything after
// that -- instantiating the graph, evaluating it, handing back the namespace
// -- happens in promise jobs, in the importer's microtask drain. What a
// program can see of that is WHEN: the importer's next statement and the
// promise jobs it queued run before the imported module's top level; the
// module's process.nextTick callbacks run after the whole drain; and the
// turn each step lands on is fixed by the shape of the chain (a module with
// no imports evaluates on the 5th turn, import() settles on the 11th, each
// level of static imports below it adds two turns). v22 linked in promise
// jobs too, which put evaluation three turns later and every level four.
//
// So this file keeps node's chain: the same async functions, the same awaits
// and the same promise plumbing, in the same order, with node's names. The
// work at each step is oam's own, done by the host through `ops` (resolve,
// load and compile, record the links, instantiate, evaluate), and keyed by
// the module's resolved path, which is oam's module identity. Nothing here
// runs a checkpoint or a tick: the host never does from inside import().
//
// What differs from node's loader, kept out on purpose:
//  - no customization hooks and no diagnostics_channel tracing (oam has
//    neither), so the chain is node's with neither registered;
//  - oam runs a CommonJS module when it loads it, to give its facade real
//    export names, where node runs it when the graph evaluates;
//  - node rewrites the message of a missing named export of a CommonJS
//    module (synchronously, from what linking recorded); oam's message is
//    V8's.
//
// A factory, not a plain script: it runs once per runtime, before any user
// code, and closes over the host's ops and the intrinsics it captures then,
// so later tampering with Promise or Map cannot reach it (node's primordials
// do the same). The host takes the function it returns and deletes the
// global.
globalThis.__oamEsmLoader = function (ops) {
  "use strict";
  const ReflectApply = Reflect.apply;
  const uncurry = (fn) => (self, ...args) => ReflectApply(fn, self, args);
  const PromiseCtor = Promise;
  const PromiseResolveFn = Promise.resolve;
  const PromiseResolve = (value) => ReflectApply(PromiseResolveFn, PromiseCtor, [value]);
  const PromisePrototypeThen = uncurry(Promise.prototype.then);
  const ArrayCtor = Array;
  const ObjectSetPrototypeOf = Object.setPrototypeOf;
  const MapCtor = Map;
  const MapPrototypeGet = uncurry(Map.prototype.get);
  const MapPrototypeSet = uncurry(Map.prototype.set);
  const SetCtor = Set;
  const SetPrototypeHas = uncurry(Set.prototype.has);
  const MapPrototypeDelete = uncurry(Map.prototype.delete);
  const SetPrototypeAdd = uncurry(Set.prototype.add);
  const isPromise = (value) => value instanceof PromiseCtor;
  const resolvedPromise = PromiseResolve();

  // primordials.SafePromiseAllReturnVoid.
  const SafePromiseAllReturnVoid = (promises, mapFn) =>
    new PromiseCtor((resolve, reject) => {
      let pendingPromises = promises.length;
      if (pendingPromises === 0) resolve();
      const onFulfilled = () => {
        if (--pendingPromises === 0) resolve();
      };
      for (let i = 0; i < promises.length; i++) {
        PromisePrototypeThen(PromiseResolve(mapFn(promises[i], i)), onFulfilled, reject);
      }
    });

  // The loader's cache of module jobs, by resolved path and "type"
  // attribute. A run starts a new module map, and with it a new cache.
  let loadCache = new MapCtor();
  let cacheGeneration;
  const cacheKey = (url, type) => `${type === undefined ? "" : type}\u0000${url}`;
  function cache() {
    const generation = ops.generation();
    if (generation !== cacheGeneration) {
      loadCache = new MapCtor();
      cacheGeneration = generation;
    }
    return loadCache;
  }
  const attributesOf = (type) => (type === undefined ? { __proto__: null } : { __proto__: null, type });

  class ModuleJob {
    constructor(url, importAttributes, module) {
      this.importAttributes = importAttributes;
      this.url = url;
      this.module = module;
      this.modulePromise = PromiseResolve(module);
      // node's constructor links at once (this.link() is syncLink off the
      // async loader hook thread): the dependency jobs, as an array.
      this.linked = this.syncLink();
      this.instantiated = undefined;
    }

    // ModuleJobBase.syncLink: the job is in the cache while its requests
    // load, so a cycle back to it finds it; the entry is taken out again
    // afterwards (on success the caller puts it back under its own key).
    syncLink() {
      const key = cacheKey(this.url, this.importAttributes.type);
      const jobs = cache();
      const hadPreviousEntry = MapPrototypeGet(jobs, key) !== undefined;
      MapPrototypeSet(jobs, key, this);
      // [specifier, type, specifier, type, ...]
      const requests = ops.requests(this.module);
      const count = requests.length / 2;
      const specifiers = ArrayCtor(count);
      ObjectSetPrototypeOf(specifiers, null);
      const modules = ArrayCtor(count);
      const evaluationDepJobs = [];
      ObjectSetPrototypeOf(evaluationDepJobs, null);
      try {
        for (let idx = 0; idx < count; idx++) {
          const specifier = requests[2 * idx];
          specifiers[idx] = specifier;
          const job = getOrCreateModuleJob(specifier, this.url, attributesOf(requests[2 * idx + 1]));
          evaluationDepJobs[evaluationDepJobs.length] = job;
          modules[idx] = job.module;
        }
        ops.link(this.module, specifiers, modules);
      } finally {
        if (!hadPreviousEntry) MapPrototypeDelete(jobs, key);
      }
      return evaluationDepJobs;
    }

    instantiate() {
      if (this.instantiated === undefined) {
        this.instantiated = this._instantiate();
      }
      return this.instantiated;
    }

    async _instantiate() {
      const jobsInGraph = new SetCtor();
      // jobsInGraph in insertion order (node iterates the set itself).
      const jobs = [];
      ObjectSetPrototypeOf(jobs, null);
      const addJobsToDependencyGraph = async (moduleJob) => {
        if (SetPrototypeHas(jobsInGraph, moduleJob)) {
          return;
        }
        SetPrototypeAdd(jobsInGraph, moduleJob);
        jobs[jobs.length] = moduleJob;
        const dependencyJobs = isPromise(moduleJob.linked) ? await moduleJob.linked : moduleJob.linked;
        return SafePromiseAllReturnVoid(dependencyJobs, addJobsToDependencyGraph);
      };
      await addJobsToDependencyGraph(this);
      ops.instantiate(this.module);
      for (let i = 0; i < jobs.length; i++) {
        jobs[i].instantiated = resolvedPromise;
      }
    }

    async run() {
      await this.instantiate();
      await ops.evaluate(this.module);
      return { __proto__: null, module: this.module };
    }
  }
  // As node's module_job.js does: a job is what an async function returns,
  // and its promise reads job.then, which Object.prototype must not supply.
  ObjectSetPrototypeOf(ModuleJob.prototype, null);

  // A module the host loaded and instantiated outside this loader -- the
  // entry's static graph, a builtin or CommonJS facade it imports, a graph a
  // require() evaluated -- is a module node's loader holds a linked,
  // instantiated job for. Adopt one the same shape, its dependencies' jobs
  // included (an instantiated module's graph is complete and instantiated).
  // A worklist, not recursion: a static graph can be deep.
  function adoptLoadedJob(url, type) {
    const adopted = [];
    ObjectSetPrototypeOf(adopted, null);
    const adopt = (jobURL, jobType) => {
      const job = { __proto__: ModuleJob.prototype };
      job.importAttributes = attributesOf(jobType);
      job.url = jobURL;
      job.module = jobURL;
      job.modulePromise = PromiseResolve(jobURL);
      job.linked = undefined;
      job.instantiated = ops.isInstantiated(jobURL) ? resolvedPromise : undefined;
      MapPrototypeSet(cache(), cacheKey(jobURL, jobType), job);
      adopted[adopted.length] = job;
      return job;
    };
    const root = adopt(url, type);
    for (let i = 0; i < adopted.length; i++) {
      const job = adopted[i];
      // [path, type, path, type, ...] -- each request's linked module.
      const deps = ops.loadedDependencies(job.url);
      const count = deps.length / 2;
      const dependencyJobs = [];
      ObjectSetPrototypeOf(dependencyJobs, null);
      for (let idx = 0; idx < count; idx++) {
        const depURL = deps[2 * idx];
        const depType = deps[2 * idx + 1];
        const cached = MapPrototypeGet(cache(), cacheKey(depURL, depType));
        dependencyJobs[idx] = cached !== undefined ? cached : adopt(depURL, depType);
      }
      // Linked synchronously, as every job off the hook thread is.
      job.linked = dependencyJobs;
    }
    return root;
  }

  function createModuleJob(url, importAttributes, specifier) {
    // Load and translate: synchronous, as node's default load is. A module
    // the host compiled but never instantiated -- left by a require() whose
    // graph failed to load or link -- is not loaded again; its job links its
    // requests afresh, so a dependency that never loaded is loaded now (a
    // CommonJS one that threw rethrows the same error) and one that did not
    // resolve fails to again.
    ops.load(url, specifier);
    const job = new ModuleJob(url, importAttributes, url);
    MapPrototypeSet(cache(), cacheKey(url, importAttributes.type), job);
    return job;
  }

  // #getOrCreateModuleJobAfterResolve.
  function getJobFromResolveResult(resolveResult, importAttributes, specifier) {
    const { url } = resolveResult;
    let job = MapPrototypeGet(cache(), cacheKey(url, importAttributes.type));
    if (job === undefined) {
      // node's load validates the attributes, and every miss of this cache
      // reaches it, a module loaded under another "type" included.
      ops.checkType(url, importAttributes.type, specifier);
      job = ops.isInstantiated(url)
        ? adoptLoadedJob(url, importAttributes.type)
        : createModuleJob(url, importAttributes, specifier);
    }
    return job;
  }

  function resolve(specifier, parentURL) {
    return { __proto__: null, url: ops.resolve(specifier, parentURL), format: undefined };
  }

  // ModuleLoader.getOrCreateModuleJob: the resolution and the job, both
  // synchronous off the async loader hook thread.
  function getOrCreateModuleJob(specifier, parentURL, importAttributes) {
    const resolveResult = resolve(specifier, parentURL);
    return getJobFromResolveResult(resolveResult, importAttributes, specifier);
  }

  // ModuleLoader.import, with its tracePromise wrapper (a plain call when
  // nothing subscribes).
  async function moduleLoaderImport(specifier, parentURL, importAttributes) {
    return (async () => {
      const moduleJob = await getOrCreateModuleJob(specifier, parentURL, importAttributes);
      const { module } = await moduleJob.run();
      return ops.namespace(module);
    })();
  }

  // The host's import() hook calls this and returns its promise.
  return async function importModuleDynamicallyCallback(specifier, referrerName, type) {
    return moduleLoaderImport(specifier, referrerName, attributesOf(type));
  };
};
