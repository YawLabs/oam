import re
src = open('crates/oam_cli/tests/e2e.rs', encoding='utf-8').read()
lines = src.split('\n')
# The TLS fin gate test's script: find its r#" ... "# block.
# Anchor on the unique line with the test fn name, then the script start after it.
start = None
for i, l in enumerate(lines):
    if 'fn the_fin_gate_holds_over_tls_for_a_get_hop_and_a_post_after_a_fin' in l:
        for j in range(i, i + 30):
            if 'r#"' in lines[j]:
                start = j
                break
        break
assert start is not None, "script start not found"
end = None
for j in range(start + 1, len(lines)):
    if lines[j].strip() == '"#':
        end = j
        break
body = lines[start:end + 1]
body[0] = body[0].split('r#"', 1)[1]
body[-1] = body[-1].rsplit('"#', 1)[0]
script = '\n'.join(body)


def grab(name):
    m = re.search(r'const ' + name + r': &str = "(.*?)";', src, re.S)
    s = m.group(1)
    s = re.sub(r'\\\s*\n\s*', '', s)
    return s.replace('\\n', '\n')


script = script.replace('__CERT__', grab('TLS_TEST_LEAF_CERT'))
script = script.replace('__KEY__', grab('TLS_TEST_LEAF_KEY'))
open('.repro/fin_ready.mjs', 'w', newline='\n').write(script)
print('bytes', len(script), '| redir_doubles:', 'redir_doubles' in script)
