// path: win32 + posix algorithms are host-independent — full battery.
import { win32 as w, posix as p } from "node:path";

console.log(w.join("C:\\a", "b", "..", "c"), w.join("\\", "host", "share"));
console.log(w.resolve("C:\\x", "y\\z"), w.resolve("C:\\base", "C:file"), w.resolve("C:\\a", "\\b"));
console.log(w.normalize("c:\\foo"), w.normalize("C:.."), w.normalize("C:\\a\\\\b\\..\\c\\"));
console.log(w.relative("C:\\a\\b", "C:\\a\\d\\e"), w.relative("C:\\x", "D:\\y"));
console.log(w.basename("C:\\dir\\file.tar.gz"), w.basename("C:\\dir\\file.txt", ".txt"), w.extname("file.tar.gz"));
console.log(w.dirname("C:\\dir\\sub\\file.ts"), w.isAbsolute("C:\\x"), w.isAbsolute("x\\y"), w.isAbsolute("\\\\srv\\sh"));
const wp = w.parse("C:\\home\\user\\file.txt");
console.log(wp.root, wp.dir, wp.base, wp.name, wp.ext);
console.log(p.join("/a", "b", "..", "c"), p.normalize("/a//b/../c/"), p.relative("/a/b", "/a/c"));
console.log(p.resolve("/x", "y", "../z"), p.isAbsolute("/q"), p.isAbsolute("q"));
const pp = p.parse("/home/user/file.txt");
console.log(pp.root, pp.dir, pp.base, pp.name, pp.ext);
console.log(w.sep, w.delimiter, p.sep, p.delimiter);
