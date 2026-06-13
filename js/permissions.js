// oam:permissions - Deno-shaped permission query/revoke surface.
//
// Registers as `oam:permissions` via __oamNode.factories, same pattern as
// oam:test.  The Rust side exposes __oam.node.permissionsQuery(name, path)
// which returns {state: "granted" | "denied"}.
//
// SNAPSHOT CONSTRAINT: top level only registers the factory; nothing here
// may touch natives, timers, or Date directly.
"use strict";
(() => {
  globalThis.__oamNode.factories["oam:permissions"] = () => {
    const node = globalThis.__oam.node;

    function assertDescriptor(desc) {
      if (typeof desc !== "object" || desc === null || typeof desc.name !== "string") {
        throw new TypeError(
          'Permission descriptor must be an object with a string "name" property'
        );
      }
    }

    /**
     * Normalise a PermissionDescriptor to {name, path?, host?}.
     * "path" and "host" (url) are the two whitelist-matching keys the Rust
     * side understands.
     */
    function normalise(desc) {
      assertDescriptor(desc);
      return {
        name: desc.name,
        path: typeof desc.path === "string" ? desc.path : undefined,
        url:  typeof desc.url  === "string" ? desc.url  : undefined,
      };
    }

    const permissions = {
      /**
       * query(descriptor) -> PermissionStatus
       * Compatible with Deno.permissions.query({name: "read", path: "/tmp"}).
       */
      query(desc) {
        const { name, path, url } = normalise(desc);
        const result = node.permissionsQuery(name, path ?? url ?? null);
        // Return a PermissionStatus-shaped object (state + eventTarget stub).
        return Promise.resolve({
          state: result.state,
          onchange: null,
          toString() { return `[PermissionStatus: ${this.state}]`; },
        });
      },

      /**
       * request(descriptor) -> PermissionStatus
       * In oam (no interactive UI), request() always returns the current
       * state (same as query).  Dynamic prompts are out of scope for M2.
       */
      request(desc) {
        return permissions.query(desc);
      },

      /**
       * revoke(descriptor) -> PermissionStatus
       * In oam M2, revoke() always returns {state: "denied"} for recognised
       * permissions; the runtime does not yet support dynamic revocation.
       * Included for API shape parity with Deno.
       */
      revoke(desc) {
        assertDescriptor(desc);
        return Promise.resolve({
          state: "denied",
          onchange: null,
          toString() { return "[PermissionStatus: denied]"; },
        });
      },
    };

    return { permissions };
  };
})();
