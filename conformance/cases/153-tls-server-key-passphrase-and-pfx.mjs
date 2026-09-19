// A TLS server's key given encrypted, with `passphrase`, or as a PKCS#12
// bundle (`pfx`): what createServer() makes of each, and -- for the ones it
// accepts -- that the server then serves real Node clients with that
// identity.
//
// The client is a separate `node` process (the harness's oracle, on PATH) in
// BOTH runs, so only the server differs between them.
//
// Regression guard: oam's tls servers read `key` as plain PEM only and
// ignored `passphrase` and `pfx`: an encrypted key, or a server built from a
// bundle, failed every handshake instead of serving, and nothing was
// reported at createServer().
//
// Fixtures (OpenSSL 3.5): the case 150 P-256 CA and localhost leaf; the
// leaf's key encrypted as PKCS#8 (AES-256-CBC / PBKDF2-HMAC-SHA256,
// AES-128-CBC / PBKDF2-HMAC-SHA1, scrypt) and as a legacy PEM key
// (AES-256-CBC, AES-128-CBC); the pair as PKCS#12 bundles (OpenSSL 3's
// default, with the CA; AES-128 with a SHA-1 MAC; no MAC; no encryption; an
// empty password; and -legacy, RC2-40, which Node 22 refuses). The
// passphrase is "hunter2" throughout. Triple-DES-protected keys and bundles
// are case 155's.
import tls from "node:tls";
import { spawn } from "node:child_process";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 50000).unref();

const CA = `-----BEGIN CERTIFICATE-----
MIIBmjCCAUGgAwIBAgIUHjF3aO/Nr2SNMEQNV9GNuumIljswCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAaMRgwFgYDVQQDDA9vYW0gaDJzIHRlc3QgQ0EwWTATBgcq
hkjOPQIBBggqhkjOPQMBBwNCAAR6EfahtynuI8VLuixWn6GiZ3BYWFdJEqP1FfLE
lCBVF/69Rm6fDrzSVP/GWO7qsNhAZmyIVWyRQJcQiBv55omto2MwYTAdBgNVHQ4E
FgQUOlIo6O4tIFNjD7vXJV51FU2DLQcwHwYDVR0jBBgwFoAUOlIo6O4tIFNjD7vX
JV51FU2DLQcwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZI
zj0EAwIDRwAwRAIgFFCfCAiuzT1cHBF7zAQEVxSrWsoco8cOD49S6whO4vsCIC/T
xtSxdoSsByDfaJz7qxOrhJzSD5lDwUdNMe3EoP9l
-----END CERTIFICATE-----
`;

const CERT = `-----BEGIN CERTIFICATE-----
MIIBvjCCAWWgAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd4wCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0bj0ngz5dpnyIRlajs
UptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1Vo4GMMIGJMBoGA1UdEQQTMBGC
CWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDATBgNVHSUE
DDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUmxnUU2rP4FwgoXrkCkeRxNgCVycwHwYD
VR0jBBgwFoAUOlIo6O4tIFNjD7vXJV51FU2DLQcwCgYIKoZIzj0EAwIDRwAwRAIg
ItB5f9aIsf9D8cXBvJvvr5ahB57RK7DgAsIVf5uJ0zcCIBPOR2Z+ycbeeByMKH2v
shKfeR1QdaoQHwJKJln0q1fo
-----END CERTIFICATE-----
`;

const KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQLidYpqFITu5wno8
Fw5b5Ahrg5eTwH0UqA7RU57egNKhRANCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0b
j0ngz5dpnyIRlajsUptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1V
-----END PRIVATE KEY-----
`;

const ENC_PKCS8_AES256_SHA256 = `-----BEGIN ENCRYPTED PRIVATE KEY-----
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBD/NfAlfZ9uL1N7wVVt
wnhUAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQ3SzJ6yMRKmFSVr12
3lg/JgSBkCYCOxV1KkXDgOlVCVslFI3opLMP2/WqW9SV8O6w22TT1dvuQZCA2M/h
fBFdD8vg+8BI5E/79HcL+4pcD/K3fC2Zt0bKlKaHfpKGGJT2McuRaLFX9QUvEv+K
neMEQmUOsMDcb+pfmQDQuVe8pUFvDFXp3IdCpdj7USyaW01vPEjygoSuwG2QRMZV
PIcp7mnM0w==
-----END ENCRYPTED PRIVATE KEY-----
`;

const ENC_PKCS8_AES128_SHA1 = `-----BEGIN ENCRYPTED PRIVATE KEY-----
MIHmMFEGCSqGSIb3DQEFDTBEMCMGCSqGSIb3DQEFDDAWBBDecnaqkm18vLtlYSAd
Ui+MAgIIADAdBglghkgBZQMEAQIEEKVfKGNqr/uy0I80w+T6g6UEgZCfws3veonN
EcFTNrXvR/pg62V6ZlVHctUFPepdsEgsJFZ5BmXn7HCqz9PkmN/R8L9pFI65Lqds
hBFPl5LX9x2UlFSl/lM/nFyiIF050ofzNGJBKL/xzqMLJFOXyFQ7KLpg8KYn97AX
BTjHkZQty/FzFKgNn3H401gDo2HwgLVmdUVW8OIKwQuWnfQIP1Rw2+Q=
-----END ENCRYPTED PRIVATE KEY-----
`;

const ENC_PKCS8_SCRYPT = `-----BEGIN ENCRYPTED PRIVATE KEY-----
MIHsMFcGCSqGSIb3DQEFDTBKMCkGCSsGAQQB2kcECzAcBBDizhYu2Ebr218D8t1X
ORUwAgJAAAIBCAIBATAdBglghkgBZQMEASoEECoZDRx3OebYeGtCW+4dKa8EgZAl
nvRYfvGg86rfYTkOJgg438VPCqX0X2e66rWWLkPWoatPxsR22GQmcCtLlSI3cl3F
adhIA1yeee+BrG+90hpjFVHchFsDpY2xmCWUp15QhLTNJ2rdXf6DzAXKYwPlrKrO
C8UQGBvR6qcOn8NJQmqINUbnjqu57GfMdUeUOcCD8yW1MHUx3vCfLZFdpnPLrmc=
-----END ENCRYPTED PRIVATE KEY-----
`;

const ENC_TRAD_AES256 = `-----BEGIN EC PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-256-CBC,596FA7EB54954C9E933AB0263BF73EFC

tmbMlvPgoBrWUneETUkHZcFnXn8SF0+YRPqL5DYe/oIIb6JdP+vZpzJ27DRhC6yf
0S/oznLFevOugicDJxR0HjfOIbIVCuaJKJdIdW/HWP4TbFjlLY3AcwTQv62GO7NF
3Re+qGxfvOX7j+u+hi0D/kyvRFVU+H0zp9zuVi7YuTM=
-----END EC PRIVATE KEY-----
`;

const ENC_TRAD_AES128 = `-----BEGIN EC PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,A7D5EF6FE5E600818D7D321D64B2B6AD

QSqMSiDDp37W+a9qRZ3bxwz77jReAONHk3hwTkzl55CYP7JbHWZNjY8imch6BYVt
cQKEff2zC/gAq7+BsEB5wVn7YC7LWWafYG4mNaLNRA43K+v0DBIogpU3qOFwQuuq
NL4MJ/XdtjGSxOUfiP6EXeIfgS5XX56ljMiWJKtnKyk=
-----END EC PRIVATE KEY-----
`;

const PFX_AES256 =
  "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";

const PFX_AES128_SHA1MAC =
  "MIIETAIBAzCCBBIGCSqGSIb3DQEHAaCCBAMEggP/MIID+zCCAqoGCSqGSIb3DQEHBqCCApswggKX" +
  "AgEAMIICkAYJKoZIhvcNAQcBMF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBBbFcg70Omw" +
  "DloTEuXgLH0cAgID6DAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBAgQQjWvHHKRJzYdMTi/87gIf" +
  "m4CCAiDwqKGC7hH9L8DRPl2ZLRqPf7v1xklZjIX5tlvRqnz1EsMdl7N0USVz9dsqVrFp31OMTsZf" +
  "KJQI3iamogGxvgGnw4MnJGGRpickK0RRu8Q58kvP2tPaYSEvfM21NVzGJwn30bEl6Xwz8BnLRhJO" +
  "iTE/B+BAPjLiGway7db5HpQeziHRKIK5aGit8MlE9icjWjDnwt417bGpBHt4yqbHLffj7kvqm0pD" +
  "PzYO3MxA72/RL8s9IMCK+A3E73m5rhktXfmqBJwflXb2tbE+2OFuosF7rjgLQwhzwdT+fX8SfRZD" +
  "eByhbojCWauJ0RjcUBb8yX/u9AoMIaW52Bs360+q/dMmTFKKjJvOMJ3bJHNxVCxA1MVEuL9V4pmX" +
  "/rhGCz6ruhUenGK+jLsAb9GLG3GpKvXYo8ancXAbF+5LKxLvDIa3R7qpGMpQweLeaaWEAL9bcFMl" +
  "YtBmHLPs2EYTmJhrWjZQTNr2aKk/5QE7YXe3QgOymHNyl0rrseY8eVSyWxX1Jw/Uj9k0XTTc3zOA" +
  "ZYJcR8LxoQHOfVnfdKn64HBnypLyM70EwFvgBOUn/Y/sDjDhy4fNkYyNxzags6aKgTHYFdsWo0TL" +
  "fh2LObQZo3dPXGe+VhsiGGlN8JNYCMxCAPyvCYE07YXAguPvPpqgjf5F7tZZ291a1prQAGO3Pjga" +
  "87ia+YGbjJ+KEqffzMQch+LvGGS2bDpTylCyOQsUHLOKVdauMIIBSQYJKoZIhvcNAQcBoIIBOgSC" +
  "ATYwggEyMIIBLgYLKoZIhvcNAQwKAQKggfcwgfQwXwYJKoZIhvcNAQUNMFIwMQYJKoZIhvcNAQUM" +
  "MCQEEE3TuIXD+brW4JeDsTuV2nECAgPoMAwGCCqGSIb3DQIJBQAwHQYJYIZIAWUDBAECBBBGFd0E" +
  "P/wZeXA18IIMQXUzBIGQOp0sj4pXYex+dHR09yJ8duUkLKs8zsbtf4cAp5qkCMco3KwhnYm+DFZO" +
  "zt9Xe14r8gECHxytbLqyDzlNhLbjbRK6q6jS0ny4TzE9R5zOiJV/EoX/oRkSwruyBPloxcDd2SaS" +
  "I8IFciOVHoyHR3nBEX0eWb5YSIJCecdED71BjvDZpsjY8SLCA27EShHFavl1MSUwIwYJKoZIhvcN" +
  "AQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4w29NMDEwITAJBgUrDgMCGgUABBRCWOY5jUX1qxAmK8pf" +
  "LL2cEGsbZgQILr2XnsrDbCkCAgPo";

const PFX_NOMAC =
  "MIIDnAIBAzCCA5UGCSqGSIb3DQEHAaCCA4YEggOCMIIDfjCCAi0GCSqGSIb3DQEHAaCCAh4EggIa" +
  "MIICFjCCAhIGCyqGSIb3DQEMCgEDoIIB2jCCAdYGCiqGSIb3DQEJFgGgggHGBIIBwjCCAb4wggFl" +
  "oAMCAQICFDsuwSw6s3PtCGc9jVhveYZ14K3eMAoGCCqGSM49BAMCMBoxGDAWBgNVBAMMD29hbSBo" +
  "MnMgdGVzdCBDQTAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowFDESMBAGA1UEAwwJ" +
  "bG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEyGUkTjD3DV3JrGpnAEOlTOTcwpEd" +
  "G49J4M+XaZ8iEZWo7FKbTf6j4rEaFSeQamY38+DEtb1TdChoO7w0aAjtVaOBjDCBiTAaBgNVHREE" +
  "EzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYI" +
  "KwYBBQUHAwEwHQYDVR0OBBYEFJsZ1FNqz+BcIKF65ApHkcTYAlcnMB8GA1UdIwQYMBaAFDpSKOju" +
  "LSBTYw+71yVedRVNgy0HMAoGCCqGSM49BAMCA0cAMEQCICLQeX/WiLH/Q/HFwbyb76+WoQee0Suw" +
  "4ALCFX+bidM3AiATzkdmfsnG3ngcjCh9r7ISn3kdUHWqEB8CSiZZ9KtX6DElMCMGCSqGSIb3DQEJ" +
  "FTEWBBQLtD/51sfNMpyVOJyVkHggeMNvTTCCAUkGCSqGSIb3DQEHAaCCAToEggE2MIIBMjCCAS4G" +
  "CyqGSIb3DQEMCgECoIH3MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBD7cB7smCSl" +
  "u1dNz/y2bAUGAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQj5YFmUxgopzmP4sQm3V4" +
  "kgSBkGLIoTrO1vyswgMJGoqeqxvarf+r9ikdAvgATeRaBhJpJ4K0Q1VG302yBD6qQy8rFESmM/CO" +
  "J5owX8wD5PIHRmQPrpsZYfxlv2cJmRYVffN+Nhp/kqSyacL5SXbDxb7IgY1tNHUIvsfir3zQVTqw" +
  "gdF1ik80p52XsLXZ9ZaAzfuhN4JFC1dOe1rWOF4XZyGeGjElMCMGCSqGSIb3DQEJFTEWBBQLtD/5" +
  "1sfNMpyVOJyVkHggeMNvTQ==";

const PFX_NOENC =
  "MIIDbQIBAzCCAyMGCSqGSIb3DQEHAaCCAxQEggMQMIIDDDCCAi0GCSqGSIb3DQEHAaCCAh4EggIa" +
  "MIICFjCCAhIGCyqGSIb3DQEMCgEDoIIB2jCCAdYGCiqGSIb3DQEJFgGgggHGBIIBwjCCAb4wggFl" +
  "oAMCAQICFDsuwSw6s3PtCGc9jVhveYZ14K3eMAoGCCqGSM49BAMCMBoxGDAWBgNVBAMMD29hbSBo" +
  "MnMgdGVzdCBDQTAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowFDESMBAGA1UEAwwJ" +
  "bG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEyGUkTjD3DV3JrGpnAEOlTOTcwpEd" +
  "G49J4M+XaZ8iEZWo7FKbTf6j4rEaFSeQamY38+DEtb1TdChoO7w0aAjtVaOBjDCBiTAaBgNVHREE" +
  "EzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYI" +
  "KwYBBQUHAwEwHQYDVR0OBBYEFJsZ1FNqz+BcIKF65ApHkcTYAlcnMB8GA1UdIwQYMBaAFDpSKOju" +
  "LSBTYw+71yVedRVNgy0HMAoGCCqGSM49BAMCA0cAMEQCICLQeX/WiLH/Q/HFwbyb76+WoQee0Suw" +
  "4ALCFX+bidM3AiATzkdmfsnG3ngcjCh9r7ISn3kdUHWqEB8CSiZZ9KtX6DElMCMGCSqGSIb3DQEJ" +
  "FTEWBBQLtD/51sfNMpyVOJyVkHggeMNvTTCB2AYJKoZIhvcNAQcBoIHKBIHHMIHEMIHBBgsqhkiG" +
  "9w0BDAoBAaCBijCBhwIBADATBgcqhkjOPQIBBggqhkjOPQMBBwRtMGsCAQEEIEC4nWKahSE7ucJ6" +
  "PBcOW+QIa4OXk8B9FKgO0VOe3oDSoUQDQgAEyGUkTjD3DV3JrGpnAEOlTOTcwpEdG49J4M+XaZ8i" +
  "EZWo7FKbTf6j4rEaFSeQamY38+DEtb1TdChoO7w0aAjtVTElMCMGCSqGSIb3DQEJFTEWBBQLtD/5" +
  "1sfNMpyVOJyVkHggeMNvTTBBMDEwDQYJYIZIAWUDBAIBBQAEIDIyeh+r5mijac76FRwsa99ud3cz" +
  "mbR2GsIVrrRR+j/YBAg55WW1SGhdgAICCAA=";

const PFX_NOPASS =
  "MIIEXAIBAzCCBBIGCSqGSIb3DQEHAaCCBAMEggP/MIID+zCCAqoGCSqGSIb3DQEHBqCCApswggKX" +
  "AgEAMIICkAYJKoZIhvcNAQcBMF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBA9OzcK4X+L" +
  "wKqlBXnLoTC9AgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQankWFtM7eT1h3w3VzJNx" +
  "CoCCAiBugLifxUz34UsVRRl9l2eij6rer9ZA5D3RvFzFJjeiyMG4z81xq5GI6sPYgxdcpFam/KR8" +
  "AyrxPeaz4AcvfGyt0K6Z+VhBSsxP5QfopLDXdMQCJWiC0ofL+1pAp/PKxA4bt59naFo0KJKuSTCv" +
  "aeP57oa7k/MmsJKrnwdV4WcM5abSZE6ca0vD5G53QbNW8j0wyKB+4YgYsowKhVX5X5chYNEDQ31E" +
  "Q16LLXc9PQZj+oU+DGo801kJ9o8yn2MTUF94RptaIAMDdk97Yxl5g/SwfrTlProSA/M9muJIpXl4" +
  "aiq8BOjt76cFAIGSnyIyNAhftpynnyCGwg8gE9KyXb4d73ZNV+/D1EoHbIAxF5Ivn7SgwWk0caw0" +
  "YAUiQG5dQwtcDfL4QD6nwZhBKqLqJ+wUuTUdNeP8fS5z1gBpKLdgDuPRezolLvzgcuEy2l4s9TV2" +
  "3JmQ7N3CkHls/E7WIWsXmh29y460ItJk4K+Gak+X4WXHFLEZXoPqye2ZWujTs4nsJ8LF54JDpprx" +
  "1wEx1nAQ3KSOQNVb5yqWKXOVH5oLSRnaTp1jfzgPeiiH7XjLMPqvNb3t6BYfNnqoeBeKPIwg4ohY" +
  "5fs/w6sQVSIqa7PvDkGyIK98Qvp9aVyFCAx2bd36OHXtMsSJYDMTs0p4ULpdgPZMprU1niox2tvj" +
  "8Ep4r0wYxCAvRZb8aCUGquL3KXu4g0+LVBTB7aimpWccnbwKMIIBSQYJKoZIhvcNAQcBoIIBOgSC" +
  "ATYwggEyMIIBLgYLKoZIhvcNAQwKAQKggfcwgfQwXwYJKoZIhvcNAQUNMFIwMQYJKoZIhvcNAQUM" +
  "MCQEEGi7U5cEkfDjG9i3tDgehnECAggAMAwGCCqGSIb3DQIJBQAwHQYJYIZIAWUDBAEqBBCBeZET" +
  "pDYcii5piMK3/I6ABIGQl6sJ7zIfeX2LOC+mAGJXCH1Z4EN/iJo1tPM1nCuwrQbwKrosweV7IEX5" +
  "/8cdMXIcIeZkQ74VM6BtFTtbplNZfG9uH3HfshNcWOIAv+C2xo3CXjvCb1yczSsSDa8qLccwBL4j" +
  "IOHHH2i0yHxMa4fJa9CleHtCLlAPtOarr4RWHmj7WraomwUQr520xx2izGMgMSUwIwYJKoZIhvcN" +
  "AQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4w29NMEEwMTANBglghkgBZQMEAgEFAAQgzVZKCOL8ajrv" +
  "qfdPMbCYORhi7bSzXfg5g2bNP+bKJlIECIS2a6C8HGvYAgIIAA==";

const PFX_LEGACY =
  "MIIDwgIBAzCCA4gGCSqGSIb3DQEHAaCCA3kEggN1MIIDcTCCAmcGCSqGSIb3DQEHBqCCAlgwggJU" +
  "AgEAMIICTQYJKoZIhvcNAQcBMBwGCiqGSIb3DQEMAQYwDgQIHFb1l3GgbRsCAggAgIICIJCKXGog" +
  "5ahV0Rnvnrps+hhBl/HebB6r+1QX4tqc8uR9VnY83kWJWSJV5SdFwQrdo4dAYjuyHNjLM8kQYHdV" +
  "ScWfrGosei7xIsnIl491yCCnIvDViLVjIzrv+vT7OyzgH5p/GcbQ8HElIUTDCwUMjkM2pD4OZCxs" +
  "pv6zAiHxai79jWRDe0k6jg0/SwFQ7tk3E2rg0TYDxlQN+byZ2Kh2lzhEKLkndnQ4NWGwmGYmsDDd" +
  "GCo+3HiI2EEU/kbcEro0HjYvoD83JoWI6fMXIixez+kn2n2XxPlmf4fYnH+0J0dbHX/NPMBBnM9P" +
  "iRyi3+Nm/w0yX7EvbKYyiRBfhGXbNiPL/TKG0QrTjHm81bPc4v9EK0wEqYucNzlHi4i3T+vAdRBR" +
  "/QZDEPE02fAjuu5h+wDHA1gAsxF0y2Yc2VbkZs8S8aRfrYE/tyMi6lX2M5aR7QzVwUmBtL8p4KuU" +
  "+aXaere7ygXZSGw86zKTJNcgjqY+T+PbnEGLqrM8bSU87yg34kmr7u3Oz0bl0H39z1NT6y1TOYc2" +
  "mGsOuRP+aBn1ogLFjfuYUAXXBOHBE9TJ96U6mY3DrOOMqzxSdnjHbohi980EjwxfzUXyV09cY512" +
  "+f1HNNKrjWZZe6p48hkZX5Go5N1jBdDSjc8xdTzwqbZWqOg4H95754slZF1eecK86SGnO7B1CSd1" +
  "vqiYYVoQV6fu8G/Cka0z2oOlEfUK5OCy5okwggECBgkqhkiG9w0BBwGggfQEgfEwge4wgesGCyqG" +
  "SIb3DQEMCgECoIG0MIGxMBwGCiqGSIb3DQEMAQMwDgQIkyb0mXIdttICAggABIGQ7PaU40D5/H/l" +
  "vQ3X/yxmd5kvqhBL+/kVVYKt+nCNBif4OiEhH1Y7BVx++nn3B68o0tz4ujGkCVhBr7ciHEkO9MGZ" +
  "0/IO2W5+DQc+oEQsCwG8W+LWAyDtFz72VZPXy/vaJ7G2JOUO/slMNM8RTc1EJEihOSP0EHn7FmGc" +
  "FRIyKlhZaN7deKk6b3KCpnE0BrRdMSUwIwYJKoZIhvcNAQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4" +
  "w29NMDEwITAJBgUrDgMCGgUABBS37T1BnrJ+DyZm5AbR8yO+OwohTAQIUrF+Vk8e3TsCAggA";

const pfx = (b64) => Buffer.from(b64, "base64");

const CLIENT = `
import tls from "node:tls";
const ops = JSON.parse(process.argv[1]);
for (const op of ops) {
  const out = await new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port: op.port, servername: "localhost", ca: op.ca }, () => {
      const peer = s.getPeerCertificate(true);
      const issuer = peer.issuerCertificate && peer.issuerCertificate !== peer ? peer.issuerCertificate.subject.CN : null;
      s.end();
      resolve({ authorized: s.authorized, subject: peer.subject.CN, issuer });
    });
    s.on("error", (e) => resolve({ error: e.code }));
  });
  console.log(JSON.stringify(out));
}
`;
function runClients(ops) {
  return new Promise((resolve) => {
    const child = spawn("node", ["--input-type=module", "-e", CLIENT, JSON.stringify(ops)], {
      stdio: ["ignore", "pipe", "inherit"],
    });
    let out = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => { out += d; });
    child.on("close", () => resolve(out.trim().split("\n").filter(Boolean)));
  });
}

const variants = [
  ["key, plain", { key: KEY, cert: CERT }],
  ["PKCS#8 AES-256 PBKDF2-SHA256", { key: ENC_PKCS8_AES256_SHA256, passphrase: "hunter2", cert: CERT }],
  ["PKCS#8 AES-128 PBKDF2-SHA1", { key: ENC_PKCS8_AES128_SHA1, passphrase: "hunter2", cert: CERT }],
  ["PKCS#8 scrypt", { key: ENC_PKCS8_SCRYPT, passphrase: "hunter2", cert: CERT }],
  ["legacy PEM AES-256", { key: ENC_TRAD_AES256, passphrase: "hunter2", cert: CERT }],
  ["legacy PEM AES-128", { key: ENC_TRAD_AES128, passphrase: "hunter2", cert: CERT }],
  ["encrypted key as a Buffer", { key: Buffer.from(ENC_PKCS8_AES256_SHA256), passphrase: "hunter2", cert: CERT }],
  ["key entry with its own passphrase", { key: [{ pem: ENC_PKCS8_AES256_SHA256, passphrase: "hunter2" }], passphrase: "nope", cert: CERT }],
  ["key entry without one, top-level passphrase", { key: [{ pem: ENC_TRAD_AES128 }], passphrase: "hunter2", cert: CERT }],
  ["wrong passphrase (PKCS#8)", { key: ENC_PKCS8_AES256_SHA256, passphrase: "nope", cert: CERT }],
  ["wrong passphrase (legacy PEM)", { key: ENC_TRAD_AES256, passphrase: "nope", cert: CERT }],
  ["no passphrase", { key: ENC_PKCS8_SCRYPT, cert: CERT }],
  ["empty passphrase", { key: ENC_PKCS8_AES128_SHA1, passphrase: "", cert: CERT }],
  ["passphrase not a string", { key: ENC_PKCS8_AES256_SHA256, passphrase: 7, cert: CERT }],
  ["pfx, OpenSSL 3 default, with the CA", { pfx: pfx(PFX_AES256), passphrase: "hunter2" }],
  ["pfx, AES-128 with a SHA-1 MAC", { pfx: pfx(PFX_AES128_SHA1MAC), passphrase: "hunter2" }],
  ["pfx, no MAC", { pfx: pfx(PFX_NOMAC), passphrase: "hunter2" }],
  ["pfx, no encryption", { pfx: pfx(PFX_NOENC), passphrase: "hunter2" }],
  ["pfx, empty password, none given", { pfx: pfx(PFX_NOPASS) }],
  ["pfx entry with its own passphrase", { pfx: [{ buf: pfx(PFX_AES256), passphrase: "hunter2" }], passphrase: "nope" }],
  ["pfx, wrong passphrase", { pfx: pfx(PFX_AES256), passphrase: "nope" }],
  ["pfx, no passphrase", { pfx: pfx(PFX_AES256) }],
  ["pfx without encryption, wrong passphrase", { pfx: pfx(PFX_NOENC), passphrase: "nope" }],
  ["pfx, legacy RC2-40", { pfx: pfx(PFX_LEGACY), passphrase: "hunter2" }],
  ["pfx not bytes", { pfx: 5 }],
  ["pfx entry passphrase not a string", { pfx: [{ buf: pfx(PFX_AES256), passphrase: 7 }] }],
];

const servers = [];
for (const [label, options] of variants) {
  let server;
  try {
    server = tls.createServer(options, (socket) => socket.end());
  } catch (e) {
    console.log(label + ": " + e.name + " " + e.code + " " + JSON.stringify(e.message) +
      " library=" + e.library + " reason=" + e.reason);
    continue;
  }
  const port = await new Promise((r) => server.listen(0, "127.0.0.1", () => r(server.address().port)));
  servers.push({ label, server, port });
}
const lines = await runClients(servers.map(({ port }) => ({ port, ca: CA })));
servers.forEach(({ label, server }, i) => {
  console.log(label + ": serves " + lines[i]);
  server.close();
});
