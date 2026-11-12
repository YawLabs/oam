// tls.TLSSocket.unref() / tls.Server.unref() release the event loop (#140),
// the tls shape of case 104: the client is unref'd BEFORE it has connected
// (node defers that to 'connect'), the listener once it is up, the accepted
// socket on 'secureConnection'. With all three unref'd the process exits on
// its own, nothing closed, every socket still reading. Self-signed pair;
// rejectUnauthorized:false because nothing here depends on verification.
import tls from "node:tls";

const cert = `-----BEGIN CERTIFICATE-----
MIIDCzCCAfOgAwIBAgIUJscRiMbEzxV45KtAxD+Lly4dJrQwDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUw
MTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQChDlr9+HgndVbQw+meASk37JFR0m2WZjpoGFX/3zjI
QiaeD38wagrRlF0HxhgarHhrArXPzYQLC2aZ1U7/vtYDiro3QIlHOYAXoE31GDOj
IpXbDWW2jA8XIHpKtWpxbUweoD/01f+DD9yS9yNy5rNh4jznuOha6VSZH+JYilI2
4Elzqh0QiR9cb1br8bJnCW61Becpprx4KUuoKb6EOeQ8EfH52gZbF7MVV458UB1A
zzA1dPuaPhA3d+IcUNAcjpSRKdW227uUAAQdj3vyCOdXKLEA9fZ1j2YH8R3hHl5I
6kl5NHttcfr5CZRwMlJoNuUzm8Hd9vfOGK0QYuj8A91BAgMBAAGjUzBRMB0GA1Ud
DgQWBBQ99/D1x3ILUtDY8uvM3shmOSkqYzAfBgNVHSMEGDAWgBQ99/D1x3ILUtDY
8uvM3shmOSkqYzAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQAH
jdkIGOWczuu/hZMJ3QZcynx2Uxm7qK3cox56uFpnO7RPkv4jBu9SAOl7WVWwOlYb
MlL7Iamv+A+NdghDucbEiDyw9798hVQggIe1cjMjkdpeXD1vBp2XWzVcFUCkQFjP
kWeSUzzcsaC/rFV5xASqP2RQK+aGAPeUfhgin1lCtuC0zfhfsQVTSD5HrPkTKYMj
7GKUJqmLqTPreCVblP9BCm9S4Og5mvNx1SFwBUIHIcuosk9daaf7JZQynYv1yJRp
E3ybo/8zVvpZ3durGRT9NLilVC289CbYbRWAkh2zmHNA5nZsgxU9/JSLAjgtmLEt
laiOwbm2oOPQ9QfligDD
-----END CERTIFICATE-----`;
const key = `-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChDlr9+HgndVbQ
w+meASk37JFR0m2WZjpoGFX/3zjIQiaeD38wagrRlF0HxhgarHhrArXPzYQLC2aZ
1U7/vtYDiro3QIlHOYAXoE31GDOjIpXbDWW2jA8XIHpKtWpxbUweoD/01f+DD9yS
9yNy5rNh4jznuOha6VSZH+JYilI24Elzqh0QiR9cb1br8bJnCW61Becpprx4KUuo
Kb6EOeQ8EfH52gZbF7MVV458UB1AzzA1dPuaPhA3d+IcUNAcjpSRKdW227uUAAQd
j3vyCOdXKLEA9fZ1j2YH8R3hHl5I6kl5NHttcfr5CZRwMlJoNuUzm8Hd9vfOGK0Q
Yuj8A91BAgMBAAECgf9+I0AgqPlx7fSQjN/rX/1oT1+BNc2efXJBFM5GGA3gye50
3K5AvMy8V/aEoCFAwtOM/BJpLgy8mbFByk6U/mGfZIdzvpfFsMMhvetQiiPnIK89
YMDIt+kZs9YTrQIw0+lKEzgECZaUj1exwt2AoC7d+tK4qZlRmm0ngFFGBw9c6g4E
bKpZPPb62HjVAPcPPNJzj0ULTCkFQ7CPhgyz7q6UQUQJ0kM/8DWbnI9qbOWkv0qN
TafdX50piyHstcXGNFelOXmUMw1qQvbPo28qpzkxH7bDU8pShzsnySJL2HL5Wxbr
PbzZ94WOOLXfD3OmT5oW9kHDpH/zUd8pKlbYkNkCgYEA1ygCE4ZSNm0B7U8iwgBK
0Aszxpnf9f4aKKfb9CsmLaf4rxmAAqRaxT4Eki2yebqRX15Ctzlf1ryddKxNAAdh
gCcc+KAdwoJOO3pkwac+r/jsmvWqXHHi/Jn9Bhj886n5NkDGfKiCBL3rUHonwOjN
7y61ExJIy46kOM81Pm88B90CgYEAv6E6owvvAjFs1eoWD5oyUnO1HeAhKvBClDjZ
dcoY965ak5RFM4Da/HcnXAho4+pJY+4O48PIi8nQeZugm8DvpOKfivxYTI3ISDmz
CG0m7N9jJiYOPyt8dpn7Yl7R8OqFvfZAd/KkJ1wBMpsKy1MGU1pdny9mVaAVjxei
fnxNprUCgYEAn2onT6wgUe8mlFwkFrX8uHT0Ydw1EqC5ZRIqaJln6kAghCxSqqJ4
FtjCrkRpjsPrXkwLBpLeLc8GoyHe03ykgz13u8d3BV1i9bLT4KA4VE4NkSsglOpV
EnBOByyQj0GLQuVvq4F3BGhrZ+96cPaNTwC+bWkIwrnnd6gffSkRw4kCgYEAsAEE
mzZdunTs0nii9IeaipJNmnf93rM3Y23nhUEut2ZDOOLowEosV8+UrfnnZNYNvCOt
N1LeAk5FFTx0QjntoVKoWH43F3DtsDCWmDmwk8UFCsfPNAPb2A7LjekrCAxO9E+V
nNWWIbRmQTWXr3G9EJeh/5AIfMKAqqF5lJTUuTUCgYEAtzMfzgUekShhJoGov7uH
MyykhATJv+3ZlR0BCuEjgb7Lu6tu/pbgD1SkhpQ3QbM+XF5DgNJWxQATcgPWP6wy
C7rRXUYQtUTmtwTetACx3EEz7k2ixAxxdDCUPJIxGcVIPVKt6sTovr3yGLMuc4f7
I5PYIZ3kyY8EsQqX4JpTtbY=
-----END PRIVATE KEY-----`;

const lines = [];
process.on("exit", (code) => {
  lines.push("exit " + code);
  process.stdout.write(lines.join("\n") + "\n");
});

const server = tls.createServer({ cert, key }, (conn) => {
  conn.on("data", () => {});
  conn.write("hello");
  conn.unref();
});
server.listen(0, "127.0.0.1", () => {
  const client = tls.connect({ host: "127.0.0.1", port: server.address().port, rejectUnauthorized: false });
  lines.push("unref before connect chained " + (client.unref() === client));
  client.on("data", (chunk) => {
    lines.push("data " + chunk);
    lines.push("readyState " + client.readyState + " pending " + client.pending);
    lines.push("server unref chained " + (server.unref() === server));
    lines.push("active " + JSON.stringify(process.getActiveResourcesInfo().filter((t) => t.startsWith("TCP"))));
  });
});
