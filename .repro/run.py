import subprocess, sys, os, time

tmpl = open('.repro/fin_tls.mjs', encoding='utf-8').read()
cert = open('.repro/leaf.pem', encoding='utf-8').read().strip()
key = open('.repro/leaf.key', encoding='utf-8').read().strip()
script = tmpl.replace('__CERT__', cert).replace('__KEY__', key)
open('.repro/fin_ready.mjs', 'w', newline='\n').write(script)

runs = int(sys.argv[1]) if len(sys.argv) > 1 else 5
env = dict(os.environ)
env['NODE_EXTRA_CA_CERTS'] = os.path.abspath('.repro/ca.pem')
env['OAM_ENABLE_NATIVE_ADDONS'] = '1'
env['OAM_CACHE_DIR'] = os.path.abspath('.repro/cache-' + str(os.getpid()))
env.pop('FORCE_COLOR', None)
env.pop('NO_COLOR', None)
env.pop('NODE_DISABLE_COLORS', None)

tgt = sys.argv[2] if len(sys.argv) > 2 else 'node'
binp = 'node' if tgt == 'node' else os.path.abspath('target/release/oam.exe')
for r in range(runs):
    t = time.time()
    argv = [binp, '.repro/fin_ready.mjs'] if tgt == 'node' else [binp, 'run', '--no-check', '.repro/fin_ready.mjs']
    p = subprocess.run(argv, env=env, capture_output=True, text=True)
    out = p.stdout.strip().replace('\n', ' ')
    print(f"[{r}] {time.time()-t:5.1f}s rc={p.returncode} {out}")
    if p.stderr.strip():
        print('   stderr:', p.stderr.strip()[:300])
