// os.networkInterfaces(), pinned against node on the same host.
//
// On Linux and macOS oam reads getifaddrs(3) the way libuv's
// uv_interface_addresses does, so everything matches node: which interfaces
// and addresses are listed, their order, each one's netmask, CIDR, hardware
// address, `internal` and IPv6 `scopeid`. It must not depend on
// /sys/class/net: the linux release leg's container has no sysfs, and oam once
// read the interface list from there and answered {} (the v0.20.0 gate).
//
// Loopback interfaces are printed whole. For the others the address itself is
// left out -- a DHCP lease or an IPv6 temporary address can change between the
// node run and the oam run -- and only whether the CIDR is that address with
// the netmask's prefix length is printed.
//
// Windows prints the loopback interface only: oam reads the adapter list
// through sysinfo there, which sorts it and leaves software adapters out
// (docs/node-divergences.md, os.networkInterfaces()).
import os from "node:os";

const interfaces = os.networkInterfaces();
const names = Object.keys(interfaces);
console.log("has a loopback interface", names.some((n) => interfaces[n].some((a) => a.internal)));

for (const name of names) {
  const addrs = interfaces[name];
  const loopback = addrs.every((a) => a.internal);
  if (process.platform === "win32" && !loopback) continue;
  if (loopback) {
    console.log(name, JSON.stringify(addrs));
    continue;
  }
  const shape = addrs.map((a) => ({
    family: a.family,
    netmask: a.netmask,
    mac: a.mac,
    internal: a.internal,
    cidrIsAddress: a.cidr === null ? null : a.cidr.startsWith(`${a.address}/`),
    prefix: a.cidr === null ? null : a.cidr.slice(a.cidr.lastIndexOf("/") + 1),
    keys: Object.keys(a).join(","),
    scopeid: a.scopeid,
  }));
  console.log(name, JSON.stringify(shape));
}
