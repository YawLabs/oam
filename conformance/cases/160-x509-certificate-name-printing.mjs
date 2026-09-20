// How a certificate's subjectAltName and authorityInfoAccess names are
// printed: X509Certificate#subjectAltName and #infoAccess, and the legacy
// object's subjectaltname and infoAccess (toLegacyObject(), which is what
// tls's getPeerCertificate() returns).
//
// Node prints a name that could be taken for more than one -- one holding a
// comma, a quote, a backslash, a control character, or (in an IA5String)
// a byte outside printable ASCII -- as a JSON string literal, so the ", "
// separated list splits back into exactly the names the certificate holds
// (the list tls.checkServerIdentity reads). A directory name is printed in
// RFC 2253 form inside such a literal; an otherName node knows is printed
// with its type; an IP address of an odd length is "<invalid length=N>".
// The legacy object's infoAccess parses a quoted location back.
//
// Regression guard: oam printed every name as it was, so a URI holding
// ", DNS:victim.test" read as a second, DNS, name.
import { X509Certificate } from "node:crypto";
import { CERTS } from "./fixtures/tls-names.mjs";

for (const [name, pem] of Object.entries(CERTS)) {
  const x509 = new X509Certificate(pem);
  const legacy = x509.toLegacyObject();
  console.log(name);
  console.log("  subjectAltName " + JSON.stringify(x509.subjectAltName));
  console.log("  subjectaltname " + JSON.stringify(legacy.subjectaltname));
  console.log("  infoAccess     " + JSON.stringify(x509.infoAccess));
  console.log("  legacy         " + JSON.stringify(legacy.infoAccess));
}
