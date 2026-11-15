# Análisis de e-lite

**Fecha:** 2026-11-15 · **Base:** `v0.1.0-alpha.29.17.0` · **Rama de la
revisión:** `review/analisis-2026-11` (sin publicar, sin etiqueta, versión sin
cambiar).

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
- Quedan nueve hallazgos abiertos nuevos (AUD-173..181). El más importante es de
  criterio, no de código: el binario publicado **aborta ante cualquier panic** y
  parte del código está escrito como si no lo hiciera.

### Alcance real de la lectura

Leído línea a línea: el pipeline de indexación (`catch_up_limited`,
`lag_guard`, `index_loop`, commit, reorganizaciones, guardia de disco), el
bucle de conexión y el reparto de lotes del servidor, el estado por cliente,
los puntos de entrada del resolver de transacciones crudas, la verificación
por txid, el commit del almacén y la exportación/importación de copias.

Revisado por estructura, búsquedas dirigidas y tests, **no línea a línea**:
`tuning.rs` (controlador adaptativo y plan de memoria), `el-blocks`, `el-chain`,
`el-mempool`, las fuentes P2P/Esplora por dentro, Tor/TLS, `setup.rs` y
`config.rs`. Las conclusiones sobre esas piezas se apoyan en su documentación,
en sus tests y en los e2e, y así hay que leerlas. Una segunda pasada debería
empezar por `tuning.rs` (3.965 líneas) y por `el-rawtx/src/p2p.rs`.

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
unidad systemd, imagen de contenedor, ARM64, exportación de copias en caliente.

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
| AUD-175 | P3 | Que un error de indexación sea fatal o se reintente para siempre se decide buscando 13 subcadenas en el mensaje (`classify_error`). | Abierto |

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
| AUD-173 | P2 | **Política de panic incoherente.** El binario publicado usa `panic = "abort"`, pero el código tiene `catch_unwind` y recuperación de *locks* envenenados que solo funcionan con *unwind*. Un panic en cualquier hilo detiene todo el nodo, y nada lo reinicia. | Abierto (decisión) |
| AUD-174 | P2 | Hasta ~130 hilos por una sola wallet activa (1 + 4 líneas × 32 hilos de lote), creados y destruidos por línea. `max_clients` limita conexiones, no hilos. | Abierto |
| AUD-180 | P3 | El Tor gestionado queda huérfano si e-lite muere sin cerrar (SIGKILL, OOM, panic) y el siguiente arranque no lo adopta ni lo para. Observado en los e2e. | Abierto |
| AUD-181 | P3 | Todos los puertos por defecto caen en el rango efímero de Linux: un `bind` puede fallar de forma esporádica. Observado una vez. | Abierto |
| AUD-178 | P3 | ROADMAP pedía respetar `client_min` en la negociación de versión; AUD-041 documenta esa misma rebaja como intencionada. Dos documentos se contradecían. | Abierto (decisión) |

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
| AUD-140 | P2 | La exportación exige parar el servicio: necesita el bloqueo de la base de datos. | Abierto |
| AUD-143 | P3 | La copia no lleva la identidad (TLS, onion): tras restaurar cambia la dirección onion y las wallets deben importar una CA nueva. | Abierto |
| AUD-176 | P3 | El digest recorre y resume **todas** las filas del índice, en un solo hilo, al exportar y otra vez al importar: el índice entero leído dos veces. | Abierto |
| — | doc | `OPERATIONS.md` decía que al restaurar no había "cambio de TLS"; en realidad se genera una CA nueva. | **Corregido** |

### Valoración

Lo que existe es correcto y prudente. Lo que falta es de alcance: hoy hay copia
de los **datos de cadena** (públicos, reconstruibles) pero no de la
**identidad** de la instancia (lo único que no se puede reconstruir), y hacer
una copia obliga a parar el servicio. Ver las propuestas 4 y 5.

## 5. Cambios hechos en esta revisión

Cuatro commits de código y uno de documentación, en la rama
`review/analisis-2026-11`.

| Cambio | Tipo | Ficheros |
|---|---|---|
| `catch_up_limited` dividido en `preflight`, `spawn_readers`, `next_prepared`, `live_lag_check`, `commit_reason`, `flush_group`, `account_call`, con `ReaderPool` y `CommitGroup` | reordenación | `bin/e-lite/src/pipeline.rs`, `bin/e-lite/src/reader_pool.rs` |
| Cierre de lectores que siempre termina (AUD-169) | arreglo P1 | `bin/e-lite/src/reader_pool.rs` |
| `lag_guard`: cuatro copias de "reanudar Core" → una; `index_loop`: dos copias de `supervisor_step` → llamada | reordenación | `bin/e-lite/src/pipeline.rs` |
| Guard para plazas de cliente, conexión muerta, análisis único de la línea, registro de errores acotado (AUD-170) | arreglo P2 | `crates/el-serve/src/server.rs`, `client.rs`, `rpc.rs` |
| `:t` solo para `.onion` y servidores locales; origen en `privacy check` (AUD-171) | arreglo P2 | `crates/el-rawtx/src/electrum.rs`, `bin/e-lite/src/privacy.rs` |
| Exportación fallida sin restos (AUD-172) | arreglo P3 | `bin/e-lite/src/snapshot.rs` |
| AUDIT solo con abiertos, ROADMAP con rutas reales, FEATURES/ELECTRUM_PROTOCOL/OPERATIONS corregidos, comprobación de referencias a código | documentación | `docs/`, `scripts/check-docs.sh` |

Único cambio visible para un usuario: una entrada `:t` hacia un servidor
clearnet en `public-servers.toml` ahora se ignora con un aviso. La plantilla
distribuida no tiene ninguna.

**Lo que decidí no cambiar**, y por qué:

- ROADMAP #10 (`client_min`): contradice una decisión documentada (AUD-041).
- ROADMAP #17, "una línea sin terminar al final de la conexión se ejecuta": no
  es un defecto. Solo se ejecuta si es un JSON completo, y hay clientes que
  cierran tras su última petición.
- ROADMAP #3 (penalización de proveedores): el arreglo es pequeño, pero la
  fuente falsa de los tests ignora `penalize`, así que no podía probarlo.
- ROADMAP #7 (límite de suscripciones): el valor del límite afecta a wallets
  grandes y necesita un dato, no una suposición.
- `catch_unwind` del ROADMAP #4: no hace nada con `panic = "abort"`.

### Verificación

Ejecutado en esta rama el 2026-11-15:

| Comprobación | Resultado |
|---|---|
| `cargo fmt --check` | correcto |
| `cargo clippy --workspace --all-targets -- -D warnings` | correcto |
| `cargo test --workspace` | 361 tests correctos, 6 ignorados (350 antes de la revisión) |
| `scripts/check-docs.sh` | correcto (38 documentos) |
| `tests/e2e/e2e_regtest.sh` | correcto |
| `tests/e2e/e2e_defaults.sh` | correcto |
| `tests/e2e/e2e_pruned_ibd.sh` (poda + SIGKILL + SIGTERM) | correcto |
| `tests/e2e/e2e_recovery.sh` | correcto |
| `tests/e2e/e2e_snapshot.sh` | correcto |
| `tests/e2e/e2e_multi_instance.sh` | **falló una vez** (`cannot bind metrics 127.0.0.1:55484`), correcto al repetir |

El fallo de `e2e_multi_instance.sh` no viene de los cambios: el puerto no tenía
ningún proceso escuchando después. Todos los puertos por defecto de e-lite
están dentro del rango efímero de Linux (32768–60999), así que una conexión
saliente de cualquier proceso puede estar usando ese puerto como origen justo
al arrancar. Es una causa probable, no demostrada; queda como AUD-181.

Los e2e dejaron además dos procesos `tor` huérfanos: el Tor gestionado sobrevive
cuando e-lite muere sin cerrar (la fase SIGKILL del test). Queda como AUD-180.

No ejecutado en esta revisión: `e2e_launch_core.sh`, `e2e_wait_core.sh`,
`e2e_external_pruned.sh`, `e2e_resilience.sh`, el test con Tor real y el
instalador.

No verificado aquí: `shellcheck` (no está instalado en esta máquina; no se
tocó ningún script de shell salvo el Python embebido de `check-docs.sh`), el
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
| P2 | Exportación de copias en caliente (AUD-140) | abierto |
| P2 | BIP352, índice derivado de gastos, empaquetado onion | abierto |
| P2 | Imagen de contenedor, matriz de versiones de Core y de wallets | abierto |
| P3 | ARM64, coordinación entre contenedores, copias incrementales | abierto |

Mediciones pendientes (de `HANDOFF.md`): carga en frío de una wallet en mainnet
sobre Tor comparando perfiles; tamaño por familia antes de AUD-130; latencia en
HDD/SATA; gestor de ARC en ZFS.

## 7. Observaciones y propuestas propias

En orden de prioridad. Cada una indica qué cambia respecto al plan escrito.

### 1. Decidir la política de panic (AUD-173) — antes que nada

Hay dos salidas coherentes y hoy el código está a medias entre ambas:

- **Mantener `abort`** (modelo "caer y recuperar"): la recuperación por WAL ya
  está probada. Entonces hay que entregar una unidad systemd con
  `Restart=on-failure` y borrar los `catch_unwind` y la recuperación de *locks*
  envenenados, que son código muerto y dan una falsa sensación de protección.
- **Pasar a `unwind`** con un *hook* que aborte solo si el hilo que falla es el
  de aplicación, commit o poda: los hilos de cara a la wallet fallan solos y el
  índice conserva la garantía actual.

Con cualquiera de las dos hay que resolver AUD-180: hoy una caída deja el Tor
gestionado huérfano.

Recomiendo la primera: es menos código y no cambia el binario. **Cambia el plan
escrito**: ROADMAP #4 proponía `catch_unwind`, que no sirve, y la unidad systemd
pasa de "P3 opcional" a P2 (ya reflejado en ROADMAP).

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
puede reconstruir, y hoy no tiene copia.

### 5. Exportación en caliente antes que copias incrementales (AUD-140)

Un *checkpoint* de RocksDB (enlaces duros) más `txids.dat` copiado hasta el
`txnum` confirmado da una copia consistente sin parar el servicio. **Cambia el
orden** del plan: una copia que obliga a parar el nodo no se hace con
regularidad, y eso importa más que hacerla incremental.

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
