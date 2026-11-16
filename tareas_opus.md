# Tareas pendientes de e-lite (encargo del dueño, 2026-11-16)

Lista de trabajo para la siguiente sesión. Está escrita para ejecutarse en
orden, sin necesitar la conversación anterior. Al terminar cada tarea, marca
su casilla aquí y haz commit; cuando no quede ninguna, borra este fichero.

## Antes de empezar

- Lee `AGENTS.md` (reglas que no se pueden romper) y `docs/HANDOFF.md`.
- Habla con el dueño **en español**. Los documentos del repositorio y los
  mensajes de commit van en inglés, salvo `docs/ANALISIS.md`.
- Estado de partida: rama `review/analisis-2026-11`, árbol limpio, 14 commits
  por delante de `main`; `main` es igual a `origin/main`. Versión
  `0.1.0-alpha.29.17.0` (último tag `v0.1.0-alpha.29.17.0`).
- **Sin systemd**, sin autenticación en Electrum, Tor por defecto: ver "Owner
  decisions" en `AGENTS.md`.
- Regla para cada cambio de código: primero un test que falle con el código
  actual, luego el arreglo. Si no puedes escribir ese test, dilo y no lo des
  por arreglado.
- Puertas de calidad antes de cada commit de código:
  ```bash
  cargo fmt --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  scripts/check-docs.sh
  ```
  y `tests/run-e2e.sh` (unos 8 minutos, necesita `bitcoind` en `PATH`) al
  cerrar cada bloque. `shellcheck` no está instalado en esta máquina: CI lo
  ejecuta; si tocas un script de shell, dilo en el resumen.
- Cada cambio de comportamiento actualiza en el mismo commit: `CHANGELOG.md`,
  la documentación afectada, `config.example.toml` si procede,
  `docs/audit/FIXED.md` (fila nueva) y quita su fila de `docs/ROADMAP.md` o
  `docs/AUDIT.md`. `scripts/check-docs.sh` falla si un documento cita una ruta
  de código que no existe.
- Fuera de alcance por decisión del dueño: **todo lo que necesite una
  medición** (IBD en mainnet, carga de wallet sobre Tor, esquema v3 /
  AUD-130, AUD-174, AUD-177, AUD-179, ROADMAP #3 y #7). No lo empieces.

## Decisiones ya tomadas por el dueño

| Tema | Decisión |
|---|---|
| Fuentes externas | Se mantienen las tres (Electrum por txid, Esplora, P2P). No proponer quitar ninguna. |
| Copia en caliente (AUD-140) | **No se quiere.** Eliminar la opción de la documentación (tarea B0). |
| Línea de comandos | Reducirla (bloque C). |
| Mediciones | No se tratan en este encargo. |
| Variantes opcionales | **Borrar las seis apagadas** (bloque D). El ARC dinámico de ZFS se mantiene. |
| AUD-178 | **Se deja como está** (tolerante). Solo documentación (tarea B0b). |

---

## Bloque A — Publicar lo que ya está listo

- [ ] **A1. Fusionar y subir.**
  1. `git checkout main && git merge --ff-only review/analisis-2026-11`.
  2. Ejecuta las puertas de calidad y `tests/run-e2e.sh` sobre `main`.
  3. `git push origin main`. El dueño lo ha pedido: no hace falta volver a
     preguntar por este push.
  4. Espera a que el workflow `ci` termine en verde
     (`gh run list --repo AlbercoinDev/e-lite --limit 3`). CI ejecuta
     `shellcheck` sobre los scripts de `tests/` reescritos, que aquí no se
     pudo pasar: si falla, corrígelo antes de seguir.

- [ ] **A2. Versión nueva.** La rama cambia comportamiento (reinicio
  automático con dos procesos, política de panic *unwind*, `bind` que
  espera), así que no puede quedarse como 29.17.0. Sigue
  `docs/RELEASING.md` al pie de la letra:
  1. `Cargo.toml` → `0.1.0-alpha.29.18.0`.
  2. En `CHANGELOG.md`, la sección `## Unreleased — 2026-11-15 review` pasa a
     `## 0.1.0-alpha.29.18.0 — <fecha>`. Deben quedar bien visibles las notas
     de actualización: `pgrep -f 'e-lite run'` devuelve dos pids, y las
     entradas `:t` hacia clearnet en `public-servers.toml` se ignoran.
  3. `docs/HANDOFF.md`: última versión, enlace al tag, "Last change".
  4. Puertas de calidad, commit `alpha.29.18.0: ...`, push de `main`, CI en
     verde.
  5. **Punto de control con el dueño antes del tag**: enséñale el resultado
     de CI y lo que no está verificado (instalador en máquina limpia,
     `e2e_networks.sh`, test con Tor real, efecto de *unwind* en velocidad) y
     pídele un sí explícito. El tag hace que el instalador de una línea
     entregue esta versión a todos los usuarios y no se puede mover.
  6. Con su sí: `git tag v0.1.0-alpha.29.18.0 && git push origin
     v0.1.0-alpha.29.18.0`, y verifica la release como dice
     `docs/RELEASING.md` sección 5 (seis artefactos, marcada Latest).

  Los bloques B y C van después, en una rama nueva desde `main`
  (`fixes/2026-11`), y saldrán en la versión siguiente.

---

## Bloque B — Arreglos verificados en el código

Un commit por tarea. Los números `#n` son las filas de "Near-term fixes" de
`docs/ROADMAP.md`.

- [ ] **B0. Retirar la copia en caliente (AUD-140).** Solo documentación.
  - `docs/AUDIT.md`: la fila AUD-140 pasa a riesgo residual aceptado:
    "export needs the service stopped; online export is not planned (owner
    decision 2026-11-16)".
  - `docs/ROADMAP.md`: borra la fila "Online snapshot export (AUD-140)" y la
    frase de la fila de copias incrementales que remite a ella.
  - `docs/ANALISIS.md`: en la sección 7, la propuesta 5 ("Exportación en
    caliente antes que copias incrementales") se marca como descartada por el
    dueño; revisa también la sección 4 y el punto 9.
  - En `docs/ANALISIS.md` sección 7.10, quita la fila 5 de la tabla (fuentes
    externas): el dueño mantiene las tres.
  - `AGENTS.md`, "Owner decisions": añade una línea "No online snapshot
    export: a backup is taken with the service stopped."
  - Comprueba que `docs/OPERATIONS.md` dice claramente que hay que parar el
    servicio para exportar.

- [ ] **B0b. Cerrar AUD-178 (negociación de versión).** Decisión del dueño
  (2026-11-16): se mantiene el comportamiento actual. No cambies código ni
  tests de `negotiate_protocol` (`crates/el-serve/src/protocol.rs`).
  - Hoy el servidor sirve la versión más alta que implementa igual o por
    debajo del máximo del cliente e ignora el mínimo; la respuesta de
    `server.version` incluye la versión negociada, así que un cliente
    estricto puede cerrar él mismo.
  - `docs/AUDIT.md`: quita la fila AUD-178 de abiertos; en AUD-041 (riesgo
    residual) añade "confirmed by the owner on 2026-11-16; `client_min` is
    not enforced".
  - `docs/ROADMAP.md`: borra la fila #10.
  - `docs/ELECTRUM_PROTOCOL.md`: que conste como desviación documentada de
    la especificación (compruébalo; añádelo si falta).
  - `docs/ANALISIS.md`: actualiza la mención de ROADMAP #10 en la sección 5.

- [ ] **B1. #1 — Los seguidores vuelven a verificar el bloque.**
  - Dónde: `crates/el-rawtx/src/lib.rs`, `external_block` (≈1363) y
    `fetch_external_block` (≈1452).
  - Defecto: el líder de una descarga compartida marca `done` y despierta a
    los seguidores antes de que el llamante ejecute `verify_full`; un lote de
    32 `transaction.get` del mismo bloque lo verifica hasta 32 veces.
  - Arreglo: el líder verifica y publica el `Arc<Block>` ya verificado antes
    de `notify_all`; los seguidores lo reutilizan. `AGENTS.md` ya dice que
    "followers reuse the leader's verified block" (AUD-165): comprueba qué
    camino no lo cumple en vez de reescribirlo entero.
  - Test (en `crates/el-rawtx/tests/resolver.rs`): una fuente falsa lenta,
    N hilos pidiendo transacciones del mismo bloque, y un contador de
    verificaciones que debe quedar en 1. Si no hay forma de contar
    verificaciones desde fuera, añade un contador en `RawtxStats`
    (`verifications`) y expónlo en `/status.rawtx`.
  - No cambies: el presupuesto único de red por descarga, la memoria de
    fallos, ni quién debe señuelos (solo el líder).

- [ ] **B2. #2 — El reintento en un circuito nuevo no ocurre.**
  - Dónde: `crates/el-rawtx/src/lib.rs` `race` (≈1622), y `endpoints` en
    `crates/el-rawtx/src/esplora.rs` (≈417) y `crates/el-rawtx/src/p2p.rs`
    (≈1142).
  - Defecto: tras un fallo transitorio de un endpoint Tor, la decisión de
    reintentar se toma mirando `endpoints()`, que ya excluye al endpoint
    recién enfriado: el "one retry on a new circuit" documentado no se da.
  - Arreglo: decidir el reintento a partir de la causa del fallo (`Blame`),
    no de la lista filtrada. Un fallo `Unreachable` o una respuesta errónea
    **no** se reintenta; un timeout/stall/corte sí, una vez, con credenciales
    SOCKS nuevas.
  - Test: `RecoveringSource` (`resolver.rs` ≈1130) u otra fuente falsa que
    falle una vez con un error transitorio y acierte a la segunda; la
    descarga debe terminar bien con exactamente 2 intentos sobre ese
    endpoint. Y el caso contrario: una respuesta errónea no se reintenta.
  - No cambies: máximo 12 intentos por carrera, `RACE_WIDTH`, ni la regla de
    que un intento abortado no culpa a su endpoint.

- [ ] **B3. #6 — Desconexión que espera a las descargas.**
  - Dónde: `crates/el-serve/src/server.rs`, `client_loop` y `wait_below`
    (≈392).
  - Defecto: `wait_below(PIPELINE_DEPTH)` bloquea al lector sin límite, y al
    desconectar la wallet el hilo de la conexión espera a las descargas en
    curso (hasta 60 s) antes de liberar su plaza.
  - Arreglo: la espera despierta cuando la conexión muere (`alive` falso o
    `shutdown`), y las líneas en segundo plano de una conexión muerta dejan
    de esperar su resultado. La descarga compartida puede seguir si otra
    conexión la quiere; lo que no puede es retener la plaza de `max_clients`.
  - Test (en `crates/el-serve/tests/protocol.rs`, junto a
    `closed_connections_release_their_slot_and_registry_entry`): fuente lenta
    (`slow_fixture`), cliente que envía 4 `transaction.get` y cierra; la
    plaza y la entrada del registro se liberan en menos de 2 s.
  - No cambies: `PIPELINE_DEPTH`, el orden de respuestas, ni la regla de no
    contestar antes con un error (AUD-045).

- [ ] **B4. #8 — `get_merkle` reconstruye el árbol en cada llamada.**
  - Dónde: `crates/el-serve/src/protocol.rs` `transaction_merkle` (≈1566) y
    `block_txids_cached` (≈1541); `crates/el-model/src/merkle.rs`.
  - Arreglo: guardar junto a los txids de los últimos 64 bloques los niveles
    del árbol ya calculados, y sacar la rama de ahí. La memoria extra por
    bloque es del orden de los propios txids: mantén el límite de 64 bloques.
  - Test: la rama devuelta es idéntica a la calculada de cero para varias
    posiciones (primera, última, impar, bloque de una sola transacción), y un
    contador o test de unidad demuestra que la segunda llamada al mismo
    bloque no vuelve a calcular.
  - La caché se invalida en un reorg igual que la de txids: comprueba cómo
    se invalida hoy `block_txids_cached` y sigue el mismo camino.

- [ ] **B5. #9 — `transaction.get` detallado mezcla dos tips en un reorg.**
  - Dónde: `crates/el-serve/src/protocol.rs`, `transaction_get` (≈1166) y
    `decorate_verbose_tx` (≈1138).
  - Defecto: toma el bloqueo de lectura dos veces; entre una y otra un reorg
    puede cambiar el bloque, y se sirven bytes que no están en el bloque
    anunciado (`blockhash`, `confirmations`).
  - Arreglo: resolver posición, cabecera y altura bajo **una** sola guarda y
    decorar con esos datos. La descarga externa sigue fuera del bloqueo
    (regla de `AGENTS.md`: nunca retener el bloqueo del índice durante una
    descarga); tras la descarga, vuelve a comprobar que el bloque sigue
    siendo el mismo y, si no, repite la resolución.
  - Test: difícil de forzar en carrera; hazlo determinista separando la
    resolución en una función que recibe la instantánea, y prueba que con una
    instantánea posterior al reorg la respuesta es coherente con ella.

- [ ] **B6. #13 — Dos plazos encadenados en una petición suelta.**
  - Dónde: `crates/el-rawtx/src/lib.rs`, `raw_tx_inner` (≈1199).
  - Defecto: una `transaction.get` puede gastar el plazo del camino por txid
    y después un plazo entero del camino de bloque completo (≈180 s).
  - Arreglo: un único plazo por petición, calculado al entrar; el camino de
    bloque recibe lo que quede. No cambies `EXTERNAL_BUDGET` ni los plazos
    por intento.
  - Test (`crates/el-rawtx/tests/txmode.rs`): fuentes falsas que agotan el
    camino por txid; el total no supera el plazo único más un margen.

- [ ] **B7. #18 — Suites e2e que faltan.** Usa `tests/lib/e2e.sh` y
  `tests/lib/e2e.py`; no copies código de otra suite. Añade cada suite a
  `docs/TESTS.md` y `tests/README.md`.
  - `privacy check` (texto y `--json`): valores efectivos y origen, con un
    `--set privacy....` que cambie uno.
  - `server.features.genesis_hash` y los puertos por defecto por red: en
    regtest ya se cubre; añade la comprobación de que `--network signet` y
    `testnet4` resuelven sus puertos (`55211/55222/55284`,
    `55111/55122/55184`) sin arrancar esas redes: basta con `e-lite doctor`
    o un test de unidad de `Config::apply_network_defaults` si ya existe
    (compruébalo antes de duplicar).
  - `esplora = []` con lista Electrum: los listeners abren sin esperar al
    sondeo y el log nombra el modo (AUD-167).
  - Snapshot: sustituye la génesis inventada del fixture de
    `bin/e-lite/src/snapshot.rs` por la de regtest real.
  - Lo que exija una red real (snapshot de signet) queda fuera: anótalo como
    no hecho.

- [ ] **B8. AUD-143 — Copia de la identidad.**
  - Problema: restaurar un snapshot cambia la dirección onion y la CA en la
    que confían las wallets, porque `tls/`, `tor/` y `decoy.key` se excluyen
    (a propósito) del snapshot.
  - Qué construir: dos subcomandos, `e-lite export-identity --output <f>` y
    `e-lite import-identity --input <f>`, **separados** del snapshot de datos.
    Contenido: `<data-dir>/tls/`, `<data-dir>/tor/` (claves del servicio
    onion), `decoy.key` y los tres ficheros de configuración (`config.toml`,
    `public-servers.toml`, `privacy.toml`).
  - Reglas:
    - `import-snapshot` sigue rechazando estos ficheros (AUD-167): no lo
      relajes.
    - El archivo se crea con permisos `0600` y la exportación avisa de que
      contiene claves privadas.
    - `import-identity` no sobrescribe nada existente salvo con `--yes`, y
      escribe con fichero temporal + rename, como hace `tls.rs`.
    - Se niega a ejecutarse con el nodo en marcha (misma comprobación que
      `export-snapshot`).
    - Cifrado: **pregunta al dueño** antes de añadir una dependencia nueva.
      Opción por defecto si no responde: sin cifrado propio, y la
      documentación recomienda cifrar el fichero con `gpg -c` o `age`.
  - Tests: ida y vuelta que deja los ficheros byte a byte iguales y con los
    mismos permisos; rechazo con nodo en marcha; rechazo de rutas fuera del
    directorio (`..`, absolutas, enlaces), reutilizando la validación de
    rutas de `snapshot.rs`. Un e2e: exportar identidad, borrar el directorio
    de datos, importar snapshot + identidad, y la huella del certificado es
    la misma (amplía `tests/e2e/e2e_snapshot.sh`).
  - Docs: `docs/OPERATIONS.md` (sección de copias), `docs/CLI.md`,
    `docs/FEATURES.md`, `docs/TOR.md`.

- [ ] **B9. AUD-175 — Errores tipados.**
  - Dónde: `bin/e-lite/src/main.rs` `classify_error` (≈155) y su uso en
    `bin/e-lite/src/pipeline.rs` (`index_loop`).
  - Defecto: que un error de indexación sea fatal o se reintente se decide
    buscando 13 subcadenas en el mensaje, y "Core is loading" con otras tres.
  - Arreglo: un tipo `IndexFailure { Permanent, Transient, CoreLoading }`
    construido donde nace el error (`el-index`, `el-store`, `el-rpc` ya
    tienen sus propios enums de error) y `classify_error` pasa a hacer
    `match` sobre la variante.
  - Método: primero **tests de caracterización** que fijen la clase de cada
    uno de los mensajes actuales (uno por subcadena), luego el cambio de
    tipos, y los tests deben seguir dando la misma clase. No cambies qué
    errores son fatales.
  - Los errores RPC de Core llegan como texto del propio Core: ahí el
    análisis del código/mensaje RPC se queda, pero encerrado en `el-rpc`.

- [ ] **B10. Diagnóstico de reinicios.**
  - El supervisor ya pasa al nodo `E_LITE_RESTARTS` y
    `E_LITE_RESTART_REASON` (`bin/e-lite/src/supervisor.rs` ≈38).
  - Añade `/status.supervisor = { supervised, restarts, last_reason }` y dos
    métricas en la tabla de `crates/el-serve/src/metrics.rs`
    (`e_lite_restarts_total` y `e_lite_supervised`).
  - Añade la clave `[safety] supervise = true` como equivalente de
    `--no-supervise`, con su línea en `config.example.toml`.
  - Test: amplía `tests/e2e/e2e_launch_core.sh`: tras el `kill -ABRT`,
    `/status.supervisor.restarts` vale 1.
  - Quita la fila "Restart diagnostics" de `docs/ROADMAP.md`.

Al cerrar el bloque B: puertas de calidad, `tests/run-e2e.sh`, y un resumen
al dueño con lo hecho, lo no hecho y lo no verificado.

---

## Bloque C — Reducir la línea de comandos

Hoy hay 115 opciones; la mayoría duplica lo que ya hace
`--set sección.clave=valor` (`apply_override`, `bin/e-lite/src/config.rs`
≈1389).

- [ ] **C1. Inventario.** Tabla con cada opción de `parse_args`
  (`bin/e-lite/src/cli.rs`): a qué clave de configuración equivale y si hace
  algo más que asignarla (mira su uso en `effective_config_with`,
  `bin/e-lite/src/setup.rs`). Las que hacen algo más **se quedan** como
  opciones.

- [ ] **C2. Opciones que se quedan** (ajusta con el inventario):
  - generales: `--config`, `--network`, `--data-dir`, `--set`, `--help`,
    `--version`, `--json`, `--log-level`;
  - Core: `--core-rpc`, `--core-datadir`, `--cookie-file`, `--rpc-user`,
    `--rpc-password`, `--manage`, `--prune-mib`;
  - puertos: `--listen`, `--tls-listen`, `--no-tls`, `--plain-lan`,
    `--metrics`;
  - privacidad: `--no-tor`, `--no-rawtx-tor`, `--allow-clearnet`;
  - proceso: `--no-supervise`, `--no-follow`;
  - permisos explícitos (no son configuración): `--allow-mainnet`,
    `--allow-low-memory`, `--allow-auto-prune`, `--yes`, `--dry-run`;
  - argumentos de subcomando: `--output`, `--input`, `--with-core`,
    `--verify`, `--expect-digest`, `--compress-level`, `--hash-files`,
    `--no-hash-files`, `--apply`, `--purge`, `--public-servers`.

- [ ] **C3. Las demás pasan a `--set`, con periodo de aviso.**
  - Una tabla `DEPRECATED_FLAGS: &[(opción, clave, tipo)]` en `cli.rs`.
    `parse_args` traduce una opción retirada a su `--set` equivalente y
    escribe **una** línea: `--readers is deprecated: use --set
    index.readers=8`. Las opciones booleanas (`--no-x`) se traducen a
    `clave=false`.
  - Se eliminan los campos correspondientes de `Args` y sus ramas en
    `effective_config_with`.
  - Precedencia: una opción retirada se comporta como un `--set` en su misma
    posición; un `--set` explícito posterior gana. Fíjalo con un test.
  - Test obligatorio: para **cada** fila de la tabla, la configuración
    efectiva con la opción antigua es igual a la obtenida con el `--set`
    equivalente. Y `help_flags_are_documented` (`bin/e-lite/src/tests.rs`)
    sigue pasando.
  - `help_text` muestra solo las opciones que se quedan y una línea que
    remite a `docs/CLI.md` para el resto.

- [ ] **C4. Documentación.** `docs/CLI.md`: las opciones que se quedan, una
  sección "Everything else: `--set`" y una tabla de equivalencias para las
  retiradas. Sustituye las opciones retiradas por `--set` en `README.md`,
  `docs/GETTING-STARTED.md`, `docs/OPERATIONS.md`, `docs/TUNING.md`, los
  scripts de `scripts/` y `tests/` (`tests/bench/bench_ibd.sh` usa varias).
  `CHANGELOG.md` con la tabla de equivalencias como nota de actualización.
  La tabla `DEPRECATED_FLAGS` se borrará dos versiones después: apúntalo en
  `docs/ROADMAP.md`.

- [ ] **C5.** `install.sh` y `scripts/e-lite-all-in-one.sh`: comprueba que
  no usan ninguna opción retirada.

---

## Bloque D — Borrar las variantes opcionales apagadas

Decisión del dueño (2026-11-16): se borran las seis variantes que están
apagadas por defecto. **El ARC dinámico de ZFS (`zfs_arc_dynamic`,
`arc-setup`) se mantiene**: está encendido por defecto y quitarlo cambiaría la
memoria en máquinas con ZFS.

Va después de los bloques B y C, un commit por variante, en este orden (de
menos a más código):

- [ ] **D1. `prefetch_deadline_ms`** (`[index]`, `--prefetch-deadline-ms`).
- [ ] **D2. `[core] cgroup_io_weight`** (`--core-cgroup-io-weight`). El
  `io_priority` por `ionice` se queda.
- [ ] **D3. `adaptive_memtables`** (`--adaptive-memtables`,
  `--no-adaptive-memtables`; `memtable_step` en
  `bin/e-lite/src/tuning/controller.rs`). **Se queda** la reducción de
  emergencia de memtables cuando `MemAvailable < 256 MiB`: es protección
  contra OOM, no la variante opcional. Sepáralas antes de borrar.
- [ ] **D4. `E_LITE_LAG_POLICY=drain`** y `E_LITE_LAG_TARGET_SECONDS`
  (`lag_guard`, `bin/e-lite/src/pipeline.rs`).
- [ ] **D5. Ventana de retraso en bloques**: `max_bulk_lag_blocks` y
  `core_resume_ahead_blocks` (`--max-bulk-lag-blocks`,
  `--core-resume-ahead-blocks`). Queda solo la ventana en GiB
  (`max_bulk_lag_gib`, `core_resume_ahead_gib`). Hoy un valor de GiB igual a 0
  activa la ventana en bloques: decide qué significa 0 a partir de ahora
  (recomendado: error de validación que nombre la clave) y pruébalo.
- [ ] **D6. `scheduler = "alternate"`** (`--scheduler`,
  `--pause-core-during-bulk` solo si deja de tener otro uso: compruébalo).
  Es la que más código toca en `pipeline.rs`, `setup.rs`,
  `core_supervisor.rs` y `/status.scheduler`. `/status.scheduler` puede
  conservar los campos de la ventana de bytes (`backlog_bytes`, `high_bytes`,
  `low_bytes`, `avg_block_bytes`), pero sin `mode`, `phase*` ni
  `index_paused` si ya nada los alimenta.

Para **cada** variante:

1. Localiza todo con `grep -rn` de la clave, la opción y la variable de
   entorno en `bin/`, `crates/`, `tests/`, `scripts/`, `docs/`,
   `config.example.toml` y `AGENTS.md`.
2. Borra la clave de `Config`, su validación, la opción de línea de comandos,
   la rama de código, sus tests y sus campos en `/status`.
3. **Una clave retirada en un `config.toml` existente no impide arrancar**:
   una lista `RETIRED_KEYS` en `bin/e-lite/src/config.rs` produce un aviso
   que nombra la clave (`[index] adaptive_memtables is no longer used; remove
   it`). Mira primero cómo trata hoy `Config` las claves desconocidas y no
   rompas ese comportamiento para las demás. Test: un fichero con las claves
   retiradas carga y avisa. Lo mismo para las opciones de línea de comandos
   retiradas: aviso, no error (encaja con la tabla `DEPRECATED_FLAGS` del
   bloque C).
4. Documentación: `config.example.toml`, `docs/TUNING.md`, `docs/CLI.md`,
   `docs/FEATURES.md`, `docs/ROADMAP.md` (fila "Core/e-lite scheduler A/B" y
   las menciones a memtables en caliente), `docs/AB_TESTS.md`, `AGENTS.md`
   (las reglas que nombran `alternate` y `drain`), `CHANGELOG.md` con nota de
   actualización.
5. Puertas de calidad; tras D4, D5 y D6, además `tests/run-e2e.sh` completo
   (`e2e_pruned_ibd`, `e2e_recovery` y `e2e_resilience` ejercitan la pausa y
   reanudación de Core).

Lo que **no** cambia: la ventana de bytes por defecto (pausa a 30 GiB,
reanuda a 7 GiB), que la marca debe mantenerse 10 s, que una reanudación por
retraso nunca anula una pausa por disco, y `pause_core_during_bulk` en modo
`overlap`.

---

## Al terminar todo

- `docs/HANDOFF.md` y `docs/ANALISIS.md` (secciones 5 y 7.10) reflejan lo
  hecho; `docs/ROADMAP.md` y `docs/AUDIT.md` ya no listan lo arreglado.
- Resumen al dueño: qué se hizo, qué no y por qué, y qué no se pudo
  verificar. No publiques la versión siguiente sin su sí.
- Borra este fichero en el último commit.
