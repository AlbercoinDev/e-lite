# e-lite — Guía desde cero y plan de pruebas

**Fecha:** 2026-11-02 · **Versión:** `0.1.0-alpha.10` + cambios actuales
**Estado:** listo para probar. La aceptación completa de signet/testnet4 queda
pendiente de ejecutarse en un host con disco suficiente (ver §5).

---

## 0. La línea que copias y pegas

Desde un checkout local (usa el código de tu working tree):

```bash
# Regtest completo: instala/actualiza, arranca Core, mina, consulta e informa
bash scripts/e-lite-all-in-one.sh --network regtest --quick --with-core --yes

# Aceptación signet completa (necesita >=15 GiB libres)
bash scripts/e-lite-all-in-one.sh --network signet --acceptance --with-core --yes

# Aceptación testnet4 completa (necesita >=20 GiB libres)
bash scripts/e-lite-all-in-one.sh --network testnet4 --acceptance --with-core --yes

# Suite local completa (launch, wait, pruned, regtest, resilience)
bash scripts/e-lite-all-in-one.sh --network regtest --suite --with-core --yes
```

Desde GitHub (si ya está publicado; clona `main`):

```bash
curl -fsSL https://raw.githubusercontent.com/AlbercoinDev/e-lite/main/scripts/e-lite-all-in-one.sh \
  | bash -s -- --network signet --acceptance --with-core --yes
```

El script hace todo:

1. Instala dependencias base y Rust si faltan.
2. Instala/actualiza Bitcoin Core con `install.sh --with-core` (o usa el existente
   con `--no-core`).
3. Compila e-lite en release y lo instala en `~/.local/bin/e-lite`.
4. Escribe una configuración en `~/e-lite-test/` (o `--run-root`).
5. Arranca e-lite, deja que e-lite lance Core con `prune=1` (manual), indexa y prueba.
6. Informa versiones, estado, altura, `pruneheight`, disco, resultado y logs en
   `~/e-lite-test/last-report.txt`.

Opciones habituales:

| Opción | Uso |
|---|---|
| `--network regtest\|signet\|testnet4\|mainnet` | red objetivo |
| `--mode quick\|suite\|acceptance` | qué prueba ejecutar |
| `--with-core` / `--no-core` | instalar/actualizar Core o usar el existente |
| `--keep` | no borrar datos de pruebas anteriores |
| `--reset` | borrar los datos de prueba antes de empezar (por defecto) |
| `--run-root <dir>` | raíz de datos de prueba |
| `--core-datadir <dir>` / `--data-dir <dir>` | directorios explícitos |
| `--prefix <dir>` | prefijo de instalación de e-lite/Core |
| `--yes` | no preguntar |

> El script rechaza `acceptance` si no hay 15 GiB (signet) o 20 GiB (testnet4)
> libres. Usa otro disco/host o libera espacio. Para forzar en un disco pequeño
> (bajo tu riesgo):
> `E_LITE_E2E_REQUIRED_FREE_GIB=0 E_LITE_E2E_MIN_FREE_GIB=1`.

---

## 1. Qué necesitas

- Linux Debian 12+ / LMDE 6+ (x86_64, glibc ≥ 2.36).
- Bitcoin Core 28 o superior (`bitcoind` y `bitcoin-cli` en `PATH`).
- Rust stable, `build-essential`, `clang`, `libclang-dev`, `pkg-config`,
  `curl` y `ca-certificates` solo si compilas desde fuente.
- `python3` para los scripts de consulta y los e2e.

### Espacio en disco (aproximado)

| Red | Core con `prune=1` | Índice e-lite | Total recomendado |
|---|---|---|---|
| regtest | < 100 MB | < 100 MB | 1 GB |
| signet | ~3–4 GB | ~6–8 GB | **≥ 15 GB libres** |
| testnet4 | ~3–5 GB | ~10–15 GB | **≥ 20 GB libres** |
| mainnet | ~15 GB | cientos de GB | disco separado para el índice |

> El guard de disco pausa las descargas de Core si cualquiera de los dos
> filesystems baja de `[pruning] min_free_gib`; el indexado sigue drenando.
> Por debajo de `critical_free_gib` también se pausa el indexado. Si Core llega a podar por delante
> del índice, el hueco es irrecuperable: hay que restaurar un snapshot o usar un
> nodo completo. No ejecutes la aceptación completa en un disco casi lleno.

---

## 2. Compilar e instalar

### Opción A — desde el código (recomendada para esta prueba)

```bash
cd /ruta/a/e-lite

# Dependencias (Debian/Ubuntu)
sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev pkg-config \
  curl ca-certificates python3

# Rust estable (si no lo tienes)
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"

cargo build --release -p e-lite
./target/release/e-lite --version
```

Copia el binario a tu `PATH` si quieres:

```bash
install -m 0755 target/release/e-lite "$HOME/.local/bin/e-lite"
```

### Opción B — instalador publicado

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
```

Instala en `~/.local/bin` por defecto y pregunta si instalar/actualizar Bitcoin
Core. Usa `--with-core --yes` para no interactivo. Para esta prueba de cambios
locales, usa la opción A.

---

## 3. Prueba rápida en regtest desde cero

Regtest no toca la red real y permite validar todo el ciclo en minutos.

### 3.1 Arrancar e-lite (e-lite lanza Core)

En una terminal:

```bash
export CORE="$HOME/e-lite-test/regtest-core"
export DATA="$HOME/e-lite-test/regtest-index"
mkdir -p "$CORE" "$DATA"

e-lite run \
  --network regtest \
  --core-datadir "$CORE" \
  --data-dir "$DATA" \
  --manage managed \
  --prune-mib 550 \
  --min-free-gib 1 \
  --listen 127.0.0.1:50001 \
  --metrics 127.0.0.1:9184
```

Qué debe pasar:

- e-lite arranca Core con `-prune=1` (manual), `-rpcbind=127.0.0.1` y el puerto 18443.
- Log de Core: `$DATA/bitcoind.log`.
- Estado inicial: `WAITING_FOR_CORE` → `BUILDING_INDEX` → `READY`.
- No hace falta `e-lite init`.

### 3.2 Minar y consultar (otra terminal)

```bash
RPC=18443
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"

# Esperar a que Core responda
until $CLI getblockcount >/dev/null 2>&1; do sleep 1; done

$CLI createwallet test
ADDR=$($CLI -rpcwallet=test getnewaddress)
$CLI -rpcwallet=test generatetoaddress 101 "$ADDR"

# Esperar a que el índice alcance el tip
until e-lite status --json 2>/dev/null | grep -q '"state": "READY"'; do sleep 1; done

# Estado
e-lite status

# Scripthash Electrum del address
SH=$(python3 - "$ADDR" "$CORE" "$RPC" <<'PY'
import hashlib, json, subprocess, sys
addr, core, rpc = sys.argv[1], sys.argv[2], sys.argv[3]
out = subprocess.check_output([
    "bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}",
    "getaddressinfo", addr,
])
print(hashlib.sha256(bytes.fromhex(json.loads(out)["scriptPubKey"])).digest()[::-1].hex())
PY
)

python3 tests/query_history.py 50001 "$SH"
```

Debe devolver ~101 entradas y `e-lite status` debe mostrar `state READY`.

### 3.3 Parar

`Ctrl-C` o `SIGTERM` al proceso de e-lite. Si `stop_on_exit=true` (por
defecto) y e-lite lanzó Core, Core se detiene también.

---

## 4. Pruebas automatizadas locales

Con `bitcoind` en `PATH` y el binario compilado:

```bash
cargo build -p e-lite

tests/e2e_launch_core.sh        # e-lite lanza Core desde datadir vacío
tests/e2e_wait_core.sh          # e-lite espera a Core y auto-inicializa
tests/e2e_external_pruned.sh    # rechaza Core pruned externo con índice vacío
tests/e2e_pruned_ibd.sh         # IBD con poda manual + prune gate
tests/e2e_regtest.sh            # Electrum, mempool, fees, reorg, snapshot
tests/e2e_resilience.sh         # caída de Core, caché, cookie, reinicio
```

Y la batería Rust:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Los e2e usan el binario `target/debug/e-lite` por defecto. Para usar el release:

```bash
E_LITE_BIN="$PWD/target/release/e-lite" tests/e2e_launch_core.sh
```

---

## 5. Aceptación completa signet / testnet4

Esta es la prueba que falta por ejecutar. Hace un IBD real desde cero, con Core
lanzado por e-lite (`prune=1` manual), sigue hasta el tip, comprueba que no hay huecos
respecto a `pruneheight` y consulta Electrum en `READY`.

### 5.1 Signet — requiere ≥15 GiB libres

```bash
cd /ruta/a/e-lite
export BIG=/mnt/disco-grande/e-lite-signet   # ajusta a tu disco
mkdir -p "$BIG/core" "$BIG/index"

E_LITE_NETWORK=signet \
E_LITE_CORE_DATADIR="$BIG/core" \
E_LITE_E2E_DATA_DIR="$BIG/index" \
E_LITE_E2E_REQUIRED_FREE_GIB=15 \
E_LITE_E2E_MIN_FREE_GIB=2 \
tests/e2e_networks.sh
```

### 5.2 Testnet4 — requiere ≥20 GiB libres

```bash
export BIG=/mnt/disco-grande/e-lite-t4
mkdir -p "$BIG/core" "$BIG/index"

E_LITE_NETWORK=testnet4 \
E_LITE_CORE_DATADIR="$BIG/core" \
E_LITE_E2E_DATA_DIR="$BIG/index" \
E_LITE_E2E_REQUIRED_FREE_GIB=20 \
E_LITE_E2E_MIN_FREE_GIB=2 \
tests/e2e_networks.sh
```

### 5.3 Qué comprueba el script

- Core arranca con `-prune=1` y el índice se construye desde 0.
- El estado pasa por `BUILDING_INDEX` (IBD) y termina en `READY` cuando
  `initialblockdownload=false` y el índice alcanza el tip.
- `indexed_height >= pruneheight` (sin hueco de poda).
- `blockchain.headers.subscribe` devuelve un header.
- `blockchain.scripthash.get_history` responde sin error.
- Imprime `signet OK` o `testnet4 OK`; en caso contrario, muestra el log.

Puertos por defecto: RPC 38332/48332, Electrum 15011, métricas 19194. Si están
ocupados, usa `E_LITE_E2E_RPC_PORT`, `E_LITE_E2E_ELECTRUM_PORT` y
`E_LITE_E2E_METRICS_PORT`.

La sincronización puede tardar desde minutos hasta horas según los pares y el
disco. El timeout por defecto es 24 h (`E_LITE_E2E_TIMEOUT`).

### 5.4 Si el disco se llena

El guard pausa Core y el indexador en `min_free_gib`; los logs muestran
`disk low` y `/status` muestra `state=PAUSE_DOWNLOAD`, `disk_low=true`. Libera
espacio; el guard reanuda en `low + (low/2).clamp(512 MiB, 5 GiB)`.

Si Core ya podó por delante del índice (`indexed_height < pruneheight`), el
estado es irrecuperable: borra el índice y repite con más disco, o restaura un
snapshot / usa un nodo completo.

### 5.5 Nota sobre el host de desarrollo actual

El host usado para desarrollar tiene ~11 GiB libres; el preflight del script
exige 15 GiB para signet y 20 GiB para testnet4, así que **rechazará la
ejecución**. Es correcto: no hay espacio para el índice completo con margen.
Usa otro disco/host o libera espacio. No ejecutes con `REQUIRED_FREE_GIB=0` salvo
que aceptes el riesgo de quedarte sin disco y perder la prueba.

---

## 6. Signet / testnet4 en modo mantenimiento

Una vez existe el índice, puedes apuntar e-lite a un Core remoto por RPC sin
volver a indexar desde cero:

```toml
[core]
manage = "external"
rpc_url = "http://127.0.0.1:38332"   # túnel SSH: ssh -L 38332:127.0.0.1:38332 host
# rpc_user = "..."
# rpc_password = "..."
```

```bash
e-lite run --config e-lite.toml
```

No se soporta IBD pruned desde cero contra un Core remoto o un Core que e-lite
no haya lanzado. Para ese caso: snapshot o nodo completo.

---

## 7. Mainnet

- Requiere `--allow-mainnet` o `[safety] allow_mainnet = true`.
- Core pruned en un disco pequeño (~15 GB) e índice en otro disco grande.
- Para producción se recomienda restaurar un snapshot y seguir el tip; un índice
  completo desde cero es I/O-bound y puede tardar mucho.
- El índice de mainnet ocupa cientos de GB; no es un modo de “disco mínimo”.

---

## 8. Monitorización y diagnóstico

```bash
e-lite status                 # resumen humano
e-lite status --json          # JSON
e-lite core status            # supervisor + RPC de Core
e-lite core logs              # últimas 50 líneas del log de Core
curl -s http://127.0.0.1:9184/status
curl -s http://127.0.0.1:9184/metrics
curl -s http://127.0.0.1:9184/healthz
```

Campos útiles de `/status`:

- `state`, `indexed_height`, `core_height`, `lag_blocks`, `retention_blocks`;
- `core.pruneheight`, `core.pruned`, `core.network_active`,
  `core.initial_block_download`;
- `db_bytes`, `disk_free_bytes`, `core_disk_free_bytes`, `disk_low`;
- `tuning`: perfil, readers, commit, batch, caché y latencias EWMA;
- `rawtx` y `mempool`.

Autotuning y recarga en caliente: [`docs/TUNING.md`](TUNING.md). Para recargar
la configuración sin reiniciar:

```bash
kill -USR1 "$(pgrep -f 'e-lite run')"
```

---

## 9. Problemas frecuentes

| Síntoma | Causa / solución |
|---|---|
| `WAITING_FOR_CORE` eterno | Core no arranca o RPC no responde. Revisa `<data-dir>/bitcoind.log`, el puerto RPC y `--core-datadir`. Con `manage=external`, arranca Core tú mismo. |
| `CORE_UNAVAILABLE` | Core caído o reiniciando. e-lite sirve caché si hay índice; espera o revisa el log. |
| `PAUSE_DOWNLOAD` + `disk_low=true` | Disco por debajo de `min_free_gib`. Libera espacio; reanuda solo. |
| `Core is pruned and the index is empty` | No se puede indexar desde cero con un Core pruned externo/remoto. Usa `manage=managed` en local, un nodo completo o un snapshot. |
| `Core is pruned from height X but the index stops at Y` | Hueco de poda irrecuperable. Restaura snapshot o reindexa con nodo completo. |
| `mainnet requires explicit opt-in` | Añade `--allow-mainnet` o `[safety] allow_mainnet = true`. |
| `[core] manage = managed needs an explicit [network] name` | Pasa `--network signet|testnet4|mainnet|regtest`; `auto` no sirve para lanzar. |
| Puerto ocupado | Cambia `--listen`/`--metrics` o los puertos del e2e. |
| `database is in use` | Ya hay un e-lite usando esa BD (RocksDB lock). Para `doctor`/`status` usa el servidor en marcha o el endpoint HTTP. |

---

## 10. Checklist de aceptación

- [ ] `cargo fmt --all --check`, `cargo clippy ... -D warnings`, `cargo test --workspace`.
- [ ] `tests/e2e_regtest.sh`, `e2e_resilience.sh`, `e2e_launch_core.sh`,
      `e2e_wait_core.sh`, `e2e_external_pruned.sh`, `e2e_pruned_ibd.sh`.
- [ ] Signet completo con ≥15 GiB libres: `signet OK`, `READY` en el tip,
      `indexed_height >= pruneheight`, consultas Electrum.
- [ ] Testnet4 completo con ≥20 GiB libres: `testnet4 OK`, mismas comprobaciones.
- [ ] Registrar los resultados en `docs/benchmarks/F2.md`.

Cuando signet y testnet4 completen, actualiza `docs/benchmarks/F2.md` con la
altura final, `pruneheight`, tamaño del índice, tiempo y espacio libre. Hasta
entonces, la aceptación completa **no** está cerrada.
