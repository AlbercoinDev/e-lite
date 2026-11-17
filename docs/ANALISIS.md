# Análisis de e-lite

**Fecha:** 2026-11-15 · **Base:** `v0.1.0-alpha.29.17.0` · **Publicado
como:** `v0.1.0-alpha.29.18.0` (rama de la revisión: `review/analisis-2026-11`).

Este documento recoge la revisión completa del proyecto: qué hace la
aplicación, el análisis en tres partes (sincronización e indexación, servicio a
las wallets, copias de seguridad), los cambios hechos durante la revisión, el
plan de mejoras que ya estaba escrito y las propuestas nuevas.

Los identificadores `AUD-nnn` remiten a [`AUDIT.md`](AUDIT.md) (abiertos) y a
[`audit/FIXED.md`](audit/FIXED.md) (corregidos).

## Resumen

- e-lite funciona y su diseño es coherente con su objetivo: un servidor Electrum
  personal sobre un Bitcoin Core **podado**, con el índice como única fuente de
  historial y todo dato externo verificado contra ese índice.
- La revisión encontró **un defecto serio**: el cierre de los hilos lectores del
  indexador podía quedarse bloqueado para siempre (AUD-169). Está corregido y
  cubierto con tests que reproducen el bloqueo.
- Se corrigieron además cuatro defectos menores del servidor, de la lista de
  servidores públicos y de las copias (AUD-170..172), y se reordenó la función
  central del indexador (de 620 a 178 líneas) sin cambiar su comportamiento.
- La documentación tenía deriva tras la última reorganización de módulos. Está
  corregida y `scripts/check-docs.sh` ahora detecta ese tipo de deriva.
- La política de panic era incoherente (AUD-173): el binario publicado abortaba
  ante cualquier panic con código escrito para no hacerlo. Por decisión del
  dueño ahora hace *unwind*: un fallo en un hilo de wallet cierra esa conexión y
  el nodo sigue; en cualquier otro hilo el proceso se detiene como antes.
- e-lite se reinicia solo tras un fallo fatal, **sin systemd ni ningún otro
  gestor de servicios** (decisión del dueño): `run` es un proceso padre que
  vigila y el nodo como hijo. El Tor gestionado ya no queda huérfano (AUD-180).
- Un fallo esporádico de arranque (`cannot bind ... Address already in use`
  sin nada escuchando) tiene un mecanismo real que ya está cubierto (AUD-181);
  que fuera la causa de los fallos vistos en los tests no está demostrado.
- Quedan seis hallazgos abiertos nuevos (AUD-174..179).

### Alcance real de la lectura

Leído línea a línea: el pipeline de indexación (`catch_up_limited`,
`lag_guard`, `index_loop`, commit, reorganizaciones, guardia de disco), el
bucle de conexión y el reparto de lotes del servidor, el estado por cliente,
los puntos de entrada del resolver de transacciones crudas, la verificación
por txid, el commit del almacén y la exportación/importación de copias.

Revisado por estructura, búsquedas dirigidas y tests, **no línea a línea**:
`tuning/` (controlador adaptativo y plan de memoria), `el-blocks`, `el-chain`,
`el-mempool`, las fuentes P2P/Esplora por dentro, Tor/TLS, `setup.rs` y
`config.rs`. Las conclusiones sobre esas piezas se apoyan en su documentación,
en sus tests y en los e2e, y así hay que leerlas. Una segunda pasada debería
empezar por `tuning/` (3.990 líneas, ya partido en `machine.rs`, `memory.rs`,
`controller.rs` y `mod.rs`) y por `el-rawtx/src/p2p.rs`.

## 1. Qué es e-lite y qué hace hoy

e-lite es un índice local para Bitcoin Core más un servidor del protocolo
Electrum 1.4–1.7. Core valida la cadena; e-lite lee la cadena activa, guarda
historial, UTXO y txids, y responde a las wallets desde ese índice.

| Área | Funcionalidad actual |
|---|---|
| Redes | mainnet (con opción explícita), testnet4, signet, regtest; una instancia por red, con puertos y directorios propios. |
| Modos | todo en uno (`run`), índice y servicio separados (`index` + `serve`), servicio sin Core (`serve` sobre un índice existente). |
| Core | local gestionado por e-lite, local externo, LAN o WAN por túnel (solo RPC); completo o podado. |
| Sincronización desde cero | IBD podado con `prune_mib = 1`: Core nunca poda solo; e-lite autoriza cada poda por debajo de `índice − 288`. |
| Índice | RocksDB con siete familias (`meta`, `blocks`, `tx_occurrences`, `history`, `utxo`, `utxo_by_script`, `undo`) y un fichero plano `txids.dat`. |
| Wallets | historial, saldo, UTXO, suscripciones, mempool, pruebas merkle, emisión de transacciones, lotes JSON-RPC, TLS con CA local, servicio onion. |
| Transacciones crudas | caché local → Core → por txid a servidores públicos con wallets señuelo (por defecto) → bloque entero. Siempre verificadas contra el índice. |
| Privacidad | Tor por defecto y cerrado ante fallo; nunca salen scripts, direcciones ni xpubs; `privacy.toml` con tres perfiles. |
| Copias | `export-snapshot` / `import-snapshot` por red, con o sin los datos de cadena de Core. |
| Reinicio automático | integrado en `run`/`serve`/`index`, sin gestor de servicios: tras un final anómalo (aborto por error interno, caída, falta de memoria) el nodo vuelve solo. |
| Operación | instalación y actualización en una línea, `/status` y `/metrics` en loopback, `doctor`, registro con tres niveles, recarga en caliente. |

Qué sale del índice local y qué necesita una fuente externa:

| Consulta | Índice local | Core | Externo |
|---|---|---|---|
| Cabeceras, historial, saldo, UTXO, pruebas merkle | sí | no | no |
| Mempool y comisiones | superposición en memoria | sí | no |
| Emisión | no | sí | no |
| Transacción cruda de un bloque que Core conserva | caché | sí | no |
| Transacción cruda de un bloque podado | caché | no | sí (txid entre señuelos, o bloque entero) |

No disponible hoy: autenticación de clientes, pagos silenciosos (BIP352),
imagen de contenedor, ARM64. La exportación de copias exige parar el servicio
(decisión del dueño: no habrá exportación en caliente).

## 2. Parte inicial: sincronización, IBD e indexación

### Cómo funciona

1. `index_loop` (`bin/e-lite/src/pipeline.rs`) llama a `catch_up_limited` con un
   máximo de 512 bloques por llamada.
2. Cada llamada empieza con `preflight`: discos, estado de Core, una pausa que
   dejó una ejecución anterior, coherencia con la poda, reorganizaciones y el
   planificador (`lag_guard`).
3. Un grupo de **lectores** (`ReaderPool`) trae cada bloque (de `blk*.dat` o por
   RPC), lo decodifica, valida su raíz merkle y resuelve sus *prevouts* contra
   la base de datos, en paralelo.
4. El hilo de **aplicación** consume los bloques en orden, construye un único
   `WriteBatch` y el *undo* de cada bloque en memoria.
5. El **commit** escribe primero `txids.dat` (con fsync) y después el lote de
   RocksDB que avanza la punta confirmada. Solo se escribe el *undo* de la
   ventana de reorganización (288 bloques).
6. La **poda** de Core solo se autoriza desde una punta ya duradera: barrera
   `SyncWAL` antes de `pruneblockchain`, que corre en su propio hilo.

### Garantías comprobadas

- **Orden de durabilidad**: `txids.dat` → lote atómico → WAL → poda. Un fallo
  entre pasos se recupera truncando `txids.dat` al valor confirmado.
- **Nunca se poda por encima de lo indexado**: el corte queda en `índice − 288`
  y usa la punta confirmada, no la aplicada.
- **Reorganizaciones**: dentro de la ventana se deshace y se reaplica; más allá
  se detiene con `NEEDS_RECOVERY`. No se reindexa en silencio.
- **Recuperación ante caída**: `tests/e2e/e2e_recovery.sh` (SIGKILL, fallo de
  escritura, reorganización) y `tests/e2e/e2e_pruned_ibd.sh` (poda + SIGKILL)
  pasan en esta rama.

### Hallazgos

| ID | Gravedad | Hallazgo | Estado |
|---|---|---|---|
| AUD-169 | P1 | **Bloqueo al parar el indexador.** Los lectores esperaban un permiso en un canal que ellos mismos mantenían abierto. Si la llamada terminaba antes de tiempo (apagado, pausa por disco, planificador alterno, error) con la ventana de bloques preparados llena, nadie los despertaba y el `join` no volvía nunca: Ctrl-C no hacía nada y una pausa por disco podía congelar la indexación. La ventana se llena siempre que aplicar es más lento que leer, por ejemplo durante un commit de varios segundos. | **Corregido** |
| AUD-179 | P3 | Los lectores se crean y destruyen en cada llamada (cada 512 bloques, y cada 500 ms en la punta) y la ventana preparada (4–32 bloques) se descarta. | Abierto |
| AUD-177 | P2 | El commit retiene el bloqueo global de escritura durante el fsync de `txids.dat` y la escritura de RocksDB; toda petición de wallet espera. Milisegundos en vivo, segundos en IBD. | Abierto |
| AUD-175 | P3 | Que un error de indexación sea fatal o se reintente para siempre se decide buscando 13 subcadenas en el mensaje (`classify_error`). | Corregido: tipo `Fatal` (2026-11-17) |

### Valoración

La lógica es sólida y está muy instrumentada; la deuda estaba en la **forma**:
una función de 620 líneas con el código posterior al commit copiado dos veces
es donde se esconden defectos como AUD-169. El planificador (`lag_guard`) sigue
siendo la pieza más difícil de razonar: cuatro políticas (bytes, bloques
heredados, drenaje, poda automática) en una sola función con estado repartido
en ocho atómicos. Funciona, pero cualquier cambio ahí necesita tests de tabla
antes de tocarlo.

Límites de escala: el coste por bloque lo domina la resolución de *prevouts*
(100–1500 ms según `AGENTS.md`), que crece con el conjunto UTXO; el índice
ocupa ~260 GiB en mainnet (AUD-130), que es el coste principal en disco.

## 3. Parte funcional: gestión de información a las wallets

### Cómo funciona

- Un hilo por conexión (`crates/el-serve/src/server.rs`). Las líneas con
  `blockchain.transaction.get` corren junto al lector (hasta 4 por conexión)
  para que una descarga lenta no bloquee el resto; los lotes se reparten entre
  8 hilos, o 32 si llevan `transaction.get`.
- Historial, saldo, UTXO y pruebas merkle salen solo del índice.
- El **mempool** es una superposición que se refresca cada 3 s pidiendo a Core
  únicamente las transacciones nuevas.
- Las **notificaciones** las despierta un commit o un cambio de mempool y se
  escriben con un límite de 2 s; un cliente que no lee se desconecta.
- Las **transacciones crudas** (`crates/el-rawtx`) siguen el orden caché →
  Core → por txid → bloque entero. En el camino por txid, cada transacción se
  comprueba contra el índice (txid, posición, rama merkle hasta la cabecera
  indexada) antes de guardarla o servirla, y viaja mezclada con wallets señuelo
  estables repartidas entre varios servidores.

### Garantías comprobadas

- El camino de entrada no fiable está cuidado: **cero** `unwrap`/`expect`/
  `panic!` fuera de tests en `el-serve`; líneas limitadas a 1 MiB, lotes a 1000
  elementos, *pongs* a 4 MiB por conexión.
- Nada externo se sirve sin verificar, y un fallo se responde como fallo: nunca
  datos inventados.
- Por defecto todo lo externo va por Tor y falla cerrado.

### Hallazgos

| ID | Gravedad | Hallazgo | Estado |
|---|---|---|---|
| AUD-170 | P2 | La plaza de `max_clients` se liberaba con código posterior a la conexión, no con un guard; una conexión marcada como muerta seguía viva hasta 600 s; el bucle decidía buscando texto en la línea cruda y volvía a analizar el JSON; el registro de errores crecía con cualquier nombre de método inventado por el cliente y escribía ese texto en el log. | **Corregido** |
| AUD-171 | P2 | La lista de servidores Electrum aceptaba texto plano (`:t`) hacia clearnet: los txids cruzarían el nodo de salida de Tor sin cifrar. Esplora ya exigía HTTPS. | **Corregido** |
| AUD-173 | P2 | **Política de panic incoherente.** El binario publicado usaba `panic = "abort"`, pero el código tenía `catch_unwind` y recuperación de *locks* envenenados que solo funcionan con *unwind*: un panic en cualquier hilo detenía todo el nodo. | **Corregido** (decisión del dueño: *unwind*) |
| AUD-174 | P2 | Hasta ~130 hilos por una sola wallet activa (1 + 4 líneas × 32 hilos de lote), creados y destruidos por línea. `max_clients` limita conexiones, no hilos. | Abierto |
| AUD-180 | P3 | El Tor gestionado quedaba huérfano si e-lite moría sin cerrar (SIGKILL, falta de memoria, aborto) y nada reiniciaba e-lite. Observado en los e2e. | **Corregido** (reinicio integrado, sin systemd) |
| AUD-181 | P2 | Los puertos por defecto caen en el rango efímero de Linux: una conexión saliente que use uno como puerto de origen (las propias llamadas RPC de e-lite a Core, por ejemplo), viva o cerrada hace menos de 60 s, hacía fallar el arranque con "Address already in use". | **Corregido** (el `bind` espera hasta 75 s) |
| AUD-178 | P3 | ROADMAP pedía respetar `client_min` en la negociación de versión; AUD-041 documenta esa misma rebaja como intencionada. Dos documentos se contradecían. | Cerrado: se mantiene la tolerancia (decisión del dueño, 2026-11-16) |

Siguen abiertos, ya verificados en ROADMAP: los seguidores de una descarga
compartida vuelven a verificar el bloque (#1), el reintento por circuito nuevo
no ocurre tras ciertos fallos (#2), un error del índice penaliza a un proveedor
sano (#3), la espera del lector no es interrumpible (#6), suscripciones sin
límite (#7), árbol merkle recalculado por llamada (#8) y `transaction.get`
detallado que puede mezclar dos puntas en una reorganización (#9).

### Valoración

Es la parte más trabajada y la más compleja. El resolver de transacciones
crudas tiene muchos temporizadores y estados (carrera de fuentes, salud por
proveedor, memoria de fallos, deuda de señuelos); cada regla tiene su motivo
documentado, pero el conjunto solo se puede validar con medidas reales sobre
Tor, que es justo la medición pendiente en `HANDOFF.md`.

Límite de escala real: no es `max_clients`, es el modelo de hilos (AUD-174) y
el bloqueo global durante los commits (AUD-177).

## 4. Parte seguridad: copias de seguridad y restauración

### Cómo funciona

- `export-snapshot` escribe un `tar` con un miembro gzip por sección. El
  manifiesto (formato 3) va también como primera entrada, de modo que una copia
  de otra red o de otro esquema se rechaza antes de extraer nada.
- La sección del índice va sin comprimir (sus ficheros ya lo están). Nunca
  incluye el directorio de Core anidado.
- `--with-core` añade solo `blocks/`, `chainstate/` e `indexes/` de esa red, con
  tamaño y SHA-256 por fichero. Wallets, claves onion/I2P y configuración de
  Core **nunca** se copian.
- `import-snapshot` exige un destino vacío, extrae en un directorio provisional
  hermano y solo lo renombra tras validar red, génesis, esquema, punta, digest y
  la coherencia con Core (`punta del índice ≤ punta de Core`,
  `altura de poda ≤ índice − 288`).
- La importación **rechaza** un archivo que contenga secretos (`decoy.key`,
  `tor/`, `tls/`, `wallets/`, configuración).

### Garantías comprobadas

- Importación atómica: un fallo no deja un destino a medias
  (`import_is_atomic_and_checks_the_expected_digest`).
- Rutas absolutas, `..`, enlaces y dispositivos dentro del `tar` se rechazan.
- `--expect-digest` detecta un archivo sustituido, no solo corrupto.
- `tests/e2e/e2e_snapshot.sh` cubre el ciclo completo con Core.

### Hallazgos

| ID | Gravedad | Hallazgo | Estado |
|---|---|---|---|
| AUD-172 | P3 | Una exportación fallida dejaba el archivo parcial `.tmp` (cientos de GiB con `--with-core`). | **Corregido** |
| AUD-140 | P2 | La exportación exige parar el servicio: necesita el bloqueo de la base de datos. | Aceptado: no habrá exportación en caliente (decisión del dueño) |
| AUD-143 | P3 | La copia no lleva la identidad (TLS, onion): tras restaurar cambia la dirección onion y las wallets deben importar una CA nueva. | Hecho como archivo aparte: `export-identity` / `import-identity` (AUD-189); queda la copia incremental |
| AUD-176 | P3 | El digest recorre y resume **todas** las filas del índice, en un solo hilo, al exportar y otra vez al importar: el índice entero leído dos veces. | Abierto |
| — | doc | `OPERATIONS.md` decía que al restaurar no había "cambio de TLS"; en realidad se genera una CA nueva. | **Corregido** |

### Valoración

Lo que existe es correcto y prudente. Lo que falta es de alcance: hoy hay copia
de los **datos de cadena** (públicos, reconstruibles) pero no de la
**identidad** de la instancia (lo único que no se puede reconstruir), y hacer
una copia obliga a parar el servicio. Ver las propuestas 4 y 5.

## 5. Cambios hechos en esta revisión

Todo está en la rama `review/analisis-2026-11`, en commits separados por
tema (`git log main..review/analisis-2026-11`).

| Cambio | Tipo | Ficheros |
|---|---|---|
| `catch_up_limited` dividido en `preflight`, `spawn_readers`, `next_prepared`, `live_lag_check`, `commit_reason`, `flush_group`, `account_call`, con `ReaderPool` y `CommitGroup` | reordenación | `bin/e-lite/src/pipeline.rs`, `bin/e-lite/src/reader_pool.rs` |
| Cierre de lectores que siempre termina (AUD-169) | arreglo P1 | `bin/e-lite/src/reader_pool.rs` |
| `lag_guard`: cuatro copias de "reanudar Core" → una; `index_loop`: dos copias de `supervisor_step` → llamada | reordenación | `bin/e-lite/src/pipeline.rs` |
| Guard para plazas de cliente, conexión muerta, análisis único de la línea, registro de errores acotado (AUD-170) | arreglo P2 | `crates/el-serve/src/server.rs`, `client.rs`, `rpc.rs` |
| `:t` solo para `.onion` y servidores locales; origen en `privacy check` (AUD-171) | arreglo P2 | `crates/el-rawtx/src/electrum.rs`, `bin/e-lite/src/privacy.rs` |
| Exportación fallida sin restos (AUD-172) | arreglo P3 | `bin/e-lite/src/snapshot.rs` |
| Reinicio automático integrado, sin systemd; Tor gestionado sin huérfanos (AUD-180) | decisión del dueño | `bin/e-lite/src/supervisor.rs`, `bin/e-lite/src/tor.rs`, `tests/e2e/e2e_launch_core.sh` |
| `bind` de los puertos que espera si solo los ocupa una conexión saliente; los tests esperan a que e-lite termine (AUD-181) | arreglo P2 | `crates/el-serve/src/server.rs`, `bin/e-lite/src/setup.rs`, `tests/e2e/` |
| Tests simplificados y reunificados: biblioteca común para los e2e (`tests/lib/e2e.sh`, `tests/lib/e2e.py`), lanzador único `tests/run-e2e.sh`, `e2e_supervisor` fusionado en `e2e_launch_core`, configuración de pruebas Rust en un solo sitio | mantenimiento | `tests/`, `crates/el-rawtx/src/lib.rs`, `crates/el-serve/tests/protocol.rs` |
| `tuning.rs` (3.965 líneas) partido en `machine.rs`, `memory.rs`, `controller.rs` y `mod.rs`; `/metrics` como tabla sobre `/status` (313 → 133 líneas), con test | reordenación | `bin/e-lite/src/tuning/`, `crates/el-serve/src/metrics.rs` |
| Política de panic: *unwind*, *hook* que contiene los hilos de wallet y aborta el resto, *mutex* sin envenenamiento (AUD-173) | decisión del dueño | `Cargo.toml`, `bin/e-lite/src/main.rs`, `crates/el-model/src/sync.rs`, `crates/el-serve/src/server.rs` |
| AUDIT solo con abiertos, ROADMAP con rutas reales, FEATURES/ELECTRUM_PROTOCOL/OPERATIONS corregidos, comprobación de referencias a código | documentación | `docs/`, `scripts/check-docs.sh` |

Cambios visibles para un usuario:

- una entrada `:t` hacia un servidor clearnet en `public-servers.toml` ahora se
  ignora con un aviso (la plantilla distribuida no tiene ninguna);
- un fallo interno al atender a una wallet cierra esa conexión en vez de
  detener el nodo;
- `run`, `serve` e `index` son ahora dos procesos (padre que vigila y nodo) y
  el nodo se reinicia solo tras un final anómalo; `--no-supervise` lo desactiva.

**Lo que decidí no cambiar**, y por qué:

- ROADMAP #10 (`client_min`): contradice una decisión documentada (AUD-041). El
  dueño confirmó después la tolerancia actual y la fila se retiró.
- ROADMAP #17, "una línea sin terminar al final de la conexión se ejecuta": no
  es un defecto. Solo se ejecuta si es un JSON completo, y hay clientes que
  cierran tras su última petición.
- ROADMAP #3 (penalización de proveedores): el arreglo es pequeño, pero la
  fuente falsa de los tests ignora `penalize`, así que no podía probarlo.
- ROADMAP #7 (límite de suscripciones): el valor del límite afecta a wallets
  grandes y necesita un dato, no una suposición.
- `catch_unwind` del ROADMAP #4: sustituido por la política de panic de la
  propuesta 1, ya aplicada.

### Verificación

Ejecutado en esta rama, última pasada completa el 2026-11-16, con el reinicio
automático activo y los tests ya reunificados:

| Comprobación | Resultado |
|---|---|
| `cargo fmt --check` | correcto |
| `cargo clippy --workspace --all-targets -- -D warnings` | correcto |
| `cargo test --workspace` | 373 tests correctos, 6 ignorados (350 antes de la revisión) |
| `tests/run-e2e.sh --release launch_core regtest` (binario release, con `panic = "unwind"`) | correcto |
| `scripts/check-docs.sh` | correcto (38 documentos) |
| `tests/run-e2e.sh`: las diez suites e2e de regtest (`defaults`, `external_pruned`, `launch_core`, `multi_instance`, `pruned_ibd`, `recovery`, `regtest`, `resilience`, `snapshot`, `wait_core`) | correctas en la última pasada (unos 8 minutos), sin procesos residuales |

Dos e2e fallaron de forma esporádica a lo largo de las pasadas
(`e2e_multi_instance.sh` dos veces con `cannot bind metrics 127.0.0.1:55484`,
`e2e_defaults.sh` una vez con conexión rechazada), más o menos una vez de cada
diez, y pasaban al repetir. Son los dos tests que usan los puertos por defecto.

- **Mecanismo real, reproducido aislado**: los puertos por defecto caen en el
  rango efímero de Linux (32768–60999). Una conexión saliente que use uno como
  puerto de origen, viva o cerrada hace menos de 60 s, impide abrir ahí un
  puerto de escucha, y las llamadas RPC de e-lite a Core crean conexiones así
  justo antes. Corregido: el `bind` espera hasta 75 s si la tabla de sockets del
  kernel no muestra a nadie escuchando, y el error dice cuál de los dos casos
  fue (AUD-181).
- **Causa de esos fallos concretos: no demostrada.** El primer arreglo no
  bastó (`multi_instance` volvió a fallar una vez) y ninguna ejecución capturó
  el momento. Además, los dos tests borraban su directorio 2 s después de
  pedir la parada, cuando las instancias aún tardan 1–2 s en terminar; ahora
  esperan a que terminen.
- **Tras los dos cambios**: 20 ejecuciones alternas de los dos tests, todas
  correctas, y en ninguna hizo falta reintentar el `bind`.

Las primeras pasadas de los e2e dejaron procesos `tor` huérfanos: el Tor
gestionado sobrevivía cuando e-lite moría sin cerrar (la fase SIGKILL de
`e2e_pruned_ibd.sh`). Era AUD-180, ya corregido.

No medido: el efecto de *unwind* sobre el tamaño del binario y la velocidad de
IBD (se espera pequeño, pero no hay medida de antes y después).

No ejecutado: `e2e_networks.sh` (sincroniza signet y testnet4 reales y se niega
a arrancar sin el espacio en disco que exige), el test con Tor real y el
instalador.

No verificado aquí: `shellcheck` (no está instalado en esta máquina, así que
los scripts de `tests/` reescritos en esta revisión no han pasado por él; CI sí
lo ejecuta), el
rendimiento de IBD en mainnet (la reordenación del pipeline se validó con tests
y e2e de regtest, no con una medición) y una carga real de wallet sobre Tor.

## 6. Plan de mejoras ya escrito

Resumen de [`ROADMAP.md`](ROADMAP.md) y [`AUDIT.md`](AUDIT.md) tal como quedan.

| Prioridad | Mejora | Estado |
|---|---|---|
| P1 | Esquema v3 del índice (AUD-130): ~260 GiB → ~130–150 GiB con referencias `txnum`, prefijos de hash y zstd en el último nivel. Exige reindexar desde cero. | aplazado por el dueño; primero medir |
| P1 | Control de acceso para Electrum fuera de loopback (AUD-010) | sin solución compatible con wallets: cortafuegos, VPN, Tor |
| P1 | RPC de Core sin TLS (AUD-011) | túnel SSH/VPN obligatorio fuera de loopback |
| P2 | Los 13 arreglos verificados que quedan en ROADMAP | abiertos |
| P2 | Rendimiento: modo secuencial para HDD, A/B del planificador alterno, controlador adaptativo v2, control por compactación | parcial |
| P2 | BIP352, índice derivado de gastos, empaquetado onion | abierto |
| P2 | Imagen de contenedor, matriz de versiones de Core y de wallets | abierto |
| P3 | ARM64, coordinación entre contenedores, copias incrementales | abierto |

Mediciones pendientes (de `HANDOFF.md`): carga en frío de una wallet en mainnet
sobre Tor comparando perfiles; tamaño por familia antes de AUD-130; latencia en
HDD/SATA; gestor de ARC en ZFS.

## 7. Observaciones y propuestas propias

En orden de prioridad. Cada una indica qué cambia respecto al plan escrito.

### 1. Política de panic (AUD-173) — aplicada

Había dos salidas coherentes y el código estaba a medias entre ambas: mantener
`abort` con reinicio externo, o pasar a *unwind* conteniendo solo los hilos de
cara a la wallet. Yo recomendaba la primera por ser menos código; **el dueño
eligió la segunda** (2026-11-16) y está hecha así:

- `panic = "unwind"` en el perfil release.
- Un *hook* instalado lo primero en `main` decide por el nombre del hilo. Los
  hilos `el-client`, `el-line`, `el-batch` y `el-fetch` fallan solos: se cierra
  esa conexión y el log registra hilo y línea de código, nunca el mensaje (podría
  citar datos de la petición). Cualquier otro hilo (índice, commit, poda,
  mempool, notificador) escribe un `FATAL` y aborta, igual que antes: un estado
  a medio aplicar nunca se sirve y la recuperación por WAL es el camino probado.
- Lo que tenía un hilo contenido lo devuelven *guards*: la plaza de cliente
  (`ClientSlot`) y el sitio en el *pipeline* de la conexión (`LinePlace`).
- Los *mutex* compartidos de `el-serve` y `el-rawtx` ya no se envenenan
  (`el_model::sync::Mutex`): con el `Mutex` estándar, un panic contenido habría
  desactivado en silencio una caché o un registro hasta reiniciar.

Probado con un test que se relanza como proceso hijo: sobrevive a un panic en
`el-client` y termina con SIGABRT ante uno en `el-prune`.

**Reinicio automático sin systemd — aplicado** (decisión del dueño,
2026-11-16). Un fallo fuera de los hilos de wallet detiene el nodo a propósito,
así que algo tiene que volver a arrancarlo. Mi primera propuesta era una unidad
systemd; se descartó para no depender de ningún gestor de servicios, y el
reinicio va dentro del propio e-lite:

- `run`, `serve` e `index` son dos procesos del mismo binario: un padre mínimo
  que solo vigila y el nodo como hijo. El padre reenvía SIGTERM, SIGINT y
  SIGUSR1.
- **Solo se reinicia un final anómalo**: el nodo murió por una señal que nadie
  envió para pararlo (el aborto tras un error interno, una caída, el *OOM
  killer*). Una salida con código, limpia o de error, es definitiva: un error
  de configuración o de integridad fallaría igual otra vez.
- Espera de 1 s que se duplica hasta 60 s; tras 5 finales anómalos en 10
  minutos el padre se detiene con una línea que lo explica.
- Core no se reinicia: sigue corriendo y el nodo nuevo lo adopta por su fichero
  de pid.
- El Tor gestionado muere con e-lite, termine como termine, y un Tor que
  hubiera dejado una ejecución anterior se para al arrancar (AUD-180).
- `kill -9` sobre el proceso que lanzó el usuario sigue matando el nodo al
  instante, como antes: los e2e de recuperación dependen de eso.
- `--no-supervise` o `E_LITE_NO_SUPERVISE=1` dejan un solo proceso.

Probado con `tests/e2e/e2e_launch_core.sh`: mata el nodo con SIGABRT y luego con
SIGKILL, y comprueba que vuelve solo, que Core conserva su pid, que el índice
sigue avanzando, que queda un único Tor y que ni una parada limpia ni una
salida con error se reinician.

Lo que e-lite no puede hacer por sí mismo es **arrancar al encender la
máquina**: eso lo lanza algo del sistema, pero sirve cualquier cosa (`cron
@reboot`, la política de reinicio de un contenedor, runit). Efecto a tener en
cuenta: `pgrep -f 'e-lite run'` devuelve ahora dos procesos; las órdenes de la
documentación usan `pkill -o`, que apunta al padre.

### 2. Lectores persistentes (AUD-179)

`ReaderPool` ya aísla los lectores. El siguiente paso es mantener un único
grupo entre llamadas y alimentarlo con alturas: desaparece el descarte por
punto de control (hasta ~3 % del trabajo de lectura con 8 lectores) y la
creación de hilos cada 500 ms en la punta. Con medición antes y después
(`tests/bench/bench_ibd.sh`, `/status.pipeline.discarded`).

### 3. Lecturas sin el bloqueo global (AUD-177) y grupo de hilos acotado (AUD-174)

Son las dos cosas que limitan cuántas wallets se pueden atender mientras el
nodo indexa. Servir lecturas desde una instantánea de RocksDB en la punta
confirmada quita la espera durante los commits; un grupo de trabajadores
compartido, dimensionado por `MachinePlan`, sustituye a los hilos por lote.
Ambas necesitan una prueba de carga de wallet antes y después: no deben
hacerse a ciegas. **No estaban en el plan escrito.**

### 4. Copia de la identidad, separada de la copia de datos (AUD-143)

El plan hablaba de "una opción que lleve la identidad" dentro de la copia. Lo
cambiaría: la copia de datos es pública y grande, la identidad es secreta y
pequeña, y la importación rechaza secretos a propósito. Propongo un archivo
aparte y cifrado (`tls/`, `tor/`, `decoy.key` y los tres ficheros de
configuración) con su propio comando. Es lo único de una instancia que no se
puede reconstruir. **Hecho** (AUD-189): `export-identity` / `import-identity`,
sin cifrado propio (no se añadió dependencia); la documentación indica cifrarlo
con `gpg -c`.

### 5. Exportación en caliente (AUD-140) — descartada por el dueño

Propuse un *checkpoint* de RocksDB para copiar sin parar el servicio. El dueño
la descartó el 2026-11-16: una copia se hace con el servicio parado. AUD-140
queda como riesgo aceptado en `AUDIT.md` y la fila sale de `ROADMAP.md`.

### 6. Digest por fichero (AUD-176)

Registrar un SHA-256 por fichero almacenado (una sola pasada secuencial,
compartida con la del `tar`) y dejar el digest canónico para `--verify=full`.
Sin romper el formato: una tabla opcional más en el manifiesto.

### 7. Errores tipados (AUD-175)

Sustituir `classify_error` por una variante creada donde nace el error. Es
barato y elimina una clase entera de fallos silenciosos (un fatal que se
reintenta para siempre).

### 8. Tests de tabla para `lag_guard` antes de tocarlo

Extraer el cálculo de umbrales a una función pura (entrada: configuración,
desfase, tamaño medio de bloque, estado; salida: pausar, reanudar o nada) y
cubrir las cuatro políticas con una tabla. Hoy solo tienen test las piezas
(`byte_window`, `bulk_lag_window`), no la decisión completa.

### 9. Sobre el orden del plan escrito

- **AUD-130 (esquema v3) debería hacerse antes de ampliar usuarios**, como ya
  apunta el dueño: cada usuario nuevo encarece un reindexado obligatorio. El
  primer paso (medir tamaño por familia) no cuesta nada y está sin hacer.
- **La medición de carga de wallet sobre Tor** condiciona varias decisiones
  (perfil por defecto, #1, #2, AUD-174, AUD-177). Es la tarea que más
  desbloquea y no requiere escribir código.
- **`scripts/check-docs.sh` como puerta permanente**: la comprobación nueva
  habría detectado las 10 referencias rotas que dejó la última reorganización.
  Conviene ampliarla a los valores por defecto citados en `AGENTS.md`, que hoy
  se mantienen a mano.

### 10. Simplificación: dónde sobra y en qué orden lo quitaría

Medido sobre la rama el 2026-11-16 (líneas sin tests): el binario tiene
20.400, `el-rawtx` 8.600, `el-serve` 5.600, y el núcleo que indexa (`el-index`,
`el-store`, `el-blocks`, `el-chain`) 5.400 entre los cuatro. Lo que más ocupa
no es indexar ni servir, sino decidir *cómo* hacerlo: 136 campos de
configuración, 115 opciones de línea de comandos y 21 variables de entorno.

Hecho en esta pasada, sin cambio de comportamiento: `tuning.rs` partido en
cuatro módulos y `/metrics` reducido a una tabla.

Dos cosas que propuse y, al leer el código, **no** hice:

- "Un solo modelo de estado para `/status`, `/metrics` y `doctor`": la premisa
  era falsa. `/metrics` ya se calcula a partir de `/status`, y `doctor` no
  muestra el estado de un nodo en marcha sino que inspecciona la máquina y la
  configuración antes de arrancar. Un tipo con `serde` para `/status` añadiría
  una definición de ~150 campos sin quitar nada.
- Partir `cmd_doctor` (503 líneas): es una lista lineal de comprobaciones que
  se lee de arriba abajo, sin estado enredado. Trocearla movería líneas sin
  hacerla más fácil de entender.

Pendiente, por orden de valor (el dueño decidió mantener las tres fuentes
externas, así que esa propuesta ya no figura):

| # | Propuesta | Qué quita | Qué necesita |
|---|---|---|---|
| 1 | Decidir o borrar las variantes opcionales: política de retraso por bloques (heredada) y `drain`, planificador `alternate`, `adaptive_memtables`, `prefetch_deadline_ms`, `cgroup_io_weight`, ARC dinámico de ZFS | código, documentación y pruebas de caminos que nadie usa por defecto | una prueba A/B en mainnet por variante |
| 2 | Línea de comandos: unas 15 opciones de uso diario; el resto solo por fichero o `--set sección.clave=valor`, que ya existe | `parse_args` (225 líneas), buena parte de `effective_config_with` (360) y de `CLI.md` | decisión del dueño: rompe compatibilidad, requiere periodo de aviso |
| 3 | Lectores persistentes entre llamadas (AUD-179) | crear y destruir hilos cada 512 bloques y cada 500 ms al día | benchmark de IBD |
| 4 | Grupo de hilos acotado en el servidor (AUD-174) | hasta ~128 hilos por conexión | medición de carga de wallet |
| 6 | **Hecho (2026-11-16)**: `AGENTS.md` pasa de 727 a 247 líneas: reglas agrupadas con su `AUD` y el documento donde está el detalle; los valores por defecto quedan solo en `docs/` | duplicación que había que mantener a mano | — |

Sobre eficiencia: según la medición de mainnet anotada en `AGENTS.md`, el
coste por bloque en la sincronización inicial lo domina la precarga de salidas
previas (100–1.500 ms frente a ~25 ms de decodificar). Ahí está la mejora
grande. No la he medido en esta revisión.
