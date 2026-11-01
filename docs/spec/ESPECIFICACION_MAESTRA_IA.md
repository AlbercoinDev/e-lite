---
project: electrum-lite-universal
artifact: especificacion-maestra-para-agente-ia
version: 1.0.0-draft
language: es
last_reviewed: 2026-11-01
status: design-only
implementation_status: not-started
source_of_truth: true
related_files:
  - AGENTS.md
  - PLAN_DE_TRABAJO.md
  - contracts/ALMACENAMIENTO.md
  - contracts/PROTOCOLO_Y_PRUEBAS.md
  - project.json
---

# Electrum-Lite Universal — Especificación maestra ejecutable para agentes de IA

**Fecha de referencia:** 1 de noviembre de 2026.  **Estado:** diseño de ingeniería, sin implementación.  **Lenguaje de trabajo:** español para documentación e incidencias; nombres de código y APIs en inglés.

> **Propósito de este documento.** Entregar a un agente programador una especificación verificable para construir, probar, medir y empaquetar una **aplicación**, no simplemente un estudio. Este archivo define el alcance; `AGENTS.md` regula la ejecución y `PLAN_DE_TRABAJO.md` contiene los tickets ordenados. Nunca interpretar una hipótesis de diseño como una función existente.

## 0. Instrucciones obligatorias para cualquier IA implementadora

1. **Leer antes de escribir:** `README.md`, este archivo, `AGENTS.md`, `contracts/ALMACENAMIENTO.md`, `contracts/PROTOCOLO_Y_PRUEBAS.md` y `PLAN_DE_TRABAJO.md`. Si hay conflicto, prevalece esta especificación; registrar una propuesta de cambio antes de alterar el contrato.
2. **No declarar funcionalidad sin pruebas.** Cada entrega debe producir código, pruebas, comandos reproducibles, salida resumida y actualización del backlog. Si no hay acceso a Bitcoin Core, informar bloqueo y ejecutar pruebas unitarias con fixtures; no fingir integración.
3. **Trabajar incrementalmente:** realizar un ticket a la vez, por orden de dependencias, y detenerse ante fallos de invariantes o de seguridad. No generar una aplicación monolítica ficticiamente 'acabada'.
4. **No comprometer fondos ni el nodo del usuario:** nunca tocar un `datadir` de Bitcoin Core existente sin ruta explícita y autorización. Usar **regtest** y directorios efímeros por defecto. No introducir semillas, claves privadas, monederos de producción ni secretos en logs.
5. **No añadir dependencia propietaria obligatoria** para la funcionalidad central. El servidor usa Bitcoin Core para validación y fuentes históricas P2P/alternativas configurables para recuperar bloques podados; no delegar la verdad del historial a servicios de exploración.
6. **Protocolo primero correcto, después compacto.** Un índice que responde incorrectamente es peor que uno grande. Optimizar solo con resultados comparativos reproducibles.
7. **No habilitar escucha pública ni poda destructiva por defecto.** Modos potencialmente peligrosos requieren configuración explícita y comprobaciones pre-vuelo.
8. **Fuentes externas:** consultar las referencias al final, versiones concretas y ayuda instalada del binario antes de implementar detalles cambiantes.

## 1. Objetivo del producto y frontera de compatibilidad

Construir **Electrum-Lite Universal**, software libre y autoalojable que proporciona un servidor Electrum para consultar, **sin pre-registrar carteras**, transacciones, saldos e historial de cualquier `scriptPubKey` que aparezca en la cadena activa de Bitcoin. Funciona con Bitcoin Core local y permite crear su BD desde dos orígenes:

- **Modo A — full-core:** Bitcoin Core con bloques históricos completos (`prune=0`), leyendo retrospectivamente desde génesis hasta el tip, y después siguiendo la cadena.
- **Modo B — pruned-IBD:** un Bitcoin Core nuevo sincroniza desde génesis en IBD con `prune=1` (poda **manual**); la aplicación indexa los bloques antes de autorizar su eliminación; al acabar y verificar integridad se puede pasar, tras parada y cambio controlado, a `prune=550`.

**Invariante de producto:** para la misma red, cadena activa y altura H, ambos modos deben generar **el mismo contenido lógico confirmado** (`logical_snapshot_hash` idéntico), aunque los ficheros de RocksDB difieran. El mempool queda fuera de esa igualdad.

**Límite esencial de universalidad:** poder consultar el historial de un script desconocido **no** implica disponer instantáneamente del `raw transaction` de todos sus TXID. Los datos crudos (sobre todo witness) requieren caché local, bloques retenidos o fuente de archivo verificable. Si la fuente no está disponible, se devuelve un error explícito y no una transacción inventada. La plena experiencia de cartera requiere resolver esa dependencia y medir su latencia/fiabilidad.

## 2. Requisitos funcionales, priorización y aceptación

| ID | Requisito verificable | Fase mínima | Prueba |
|---|---|---|---|
| RF-001 | Leer bloques activos de Core completo desde altura 0, sin `txindex=1` | F1 | Integración regtest con Core sin `txindex` |
| RF-002 | Construir historial de **todos** los scripts observados, sin registro previo | F1 | Importar script nuevo después de indexar |
| RF-003 | Vincular cada gasto al script/importe de su `prevout` | F1 | Fixtures de gasto, múltiples salidas, cambio |
| RF-004 | Mantener UTXO confirmado, importes exactos en satoshis y saldos por script | F1 | Oráculo independiente, sin floats |
| RF-005 | Conservar orden de TXID por bloque y responder pruebas Merkle | F2 | Verificar raíz de cabecera en bloques pares/impares |
| RF-006 | Consultar una transacción raw mediante Core, caché o archivo verificable | F4 | Raw con y sin witness; no aceptar raw inválido |
| RF-007 | Indexar a la vez que IBD con poda **manual segura** | F3 | Prueba B con apagados y atrasos de indexador |
| RF-008 | A/B misma BD lógica, mismo tip y mismas respuestas confirmadas | F3 | Fingerprint determinista + consultas de muestra |
| RF-009 | Servir interfaz Electrum JSON-RPC negociando versión **1.4** inicialmente | F2/F4 | Pruebas automatizadas de protocolo |
| RF-010 | Notificaciones, mempool, emisión y consulta de tarifas según versión | F5 | Pruebas de mempool y clientes reales |
| RF-011 | Manejar reorganizaciones con journal reverso y reconstrucción controlada | F2/F3 | Reorg de múltiples bloques + reinicios |
| RF-012 | Exportar/importar snapshot validado y formato BD versionado | F5 | Restauración + fingerprint correcto |
| RF-013 | Límite de disco, pausa del IBD, métricas, recuperación tras crash | F3/F5 | Fault injection y cuota artificial |
| RF-014 | Compatibilidad con Electrum y Sparrow en conexión privada | F5 | Matriz de versiones y casos de restauración |
| RF-015 | Evolucionar a protocolo Electrum 1.7 si se verifica negociación | F6 | Pruebas específicas de `scriptpubkey.*` |

**No funcionales:** (a) no pérdida silenciosa de registros; (b) límites configurables de RAM, disco, concurrencia y solicitudes; (c) API de consulta coherente con un `indexed_tip`; (d) tareas idempotentes; (e) observabilidad y documentación de recuperación; (f) licencia compatible con el conjunto de dependencias elegido (verificar, no asumir); (g) servicio Linux Debian sin privilegios de root.

**Fuera de alcance inicial:** cartera custodial, almacenamiento de semillas/llaves, explorador web, Lightning, wallet registration, proveedores centralizados obligatorios, compatibilidad pública de producción, soporte multi-red dentro de la misma base de datos y promesa de un tamaño fijo como 64/128 GB en mainnet.

## 3. Decisiones de arquitectura (ADR actuales)

| ADR | Decisión | Motivo | Estado |
|---|---|---|---|
| ADR-001 | Rust estable, crates modularizados | Seguridad de memoria, rendimiento y portabilidad | Aceptada como propuesta |
| ADR-002 | RocksDB inicialmente, detrás de un trait `Storage` | Atomic WriteBatch / iteradores; sustituible | A probar |
| ADR-003 | `tx_ref` de **u64** en v1, no 5 bytes | Correctitud e instrumentación antes de compresión | Aceptada |
| ADR-004 | Modo B con `prune=1`, **no** `prune=550` automático durante IBD | Barrera explícita antes de poda | Aceptada |
| ADR-005 | TXID por bloque en orden, más búsqueda `txid -> ocurrencias` | Merkle sin bloque antiguo; compatibilidad BIP30 | Aceptada |
| ADR-006 | UTXO local + prevouts de Core como optimización verificable | No perder script de entrada | Aceptada |
| ADR-007 | Core es autoridad de validación y cadena activa; raw externo debe verificarse | Evitar aceptar datos de pares no confiables | Aceptada |
| ADR-008 | Electrum **1.4** como objetivo inicial, 1.7 como ampliación | Reducir superficie de interoperabilidad y errores | Aceptada |
| ADR-009 | Una BD por red y bloque génesis; bloquear si no coincide | Evitar contaminación de cadenas | Aceptada |
| ADR-010 | Indexación cronológica duradera, mempool como overlay efímero | Semántica y crash recovery claros | Aceptada |
| ADR-011 | El fichero de BD **no** es el contrato portable; sí una exportación canónica | Independencia de engine/compaction | Aceptada |

## 4. Arquitectura y límites de confianza

```text
                +--------------------------------------+
                | Bitcoin Core (validacion consenso)  |
                | RPC cookie; REST local opcional     |
                +----------------+---------------------+
                                 |
                      read_block / tip / prevouts
                                 |
 +-------------------------------v-------------------------------+
 | core-adapter -> supervisor -> indexer -> storage (RocksDB)     |
 |                     |           |        |                     |
 |             manual-prune-gate   +--> UTXO, history, TXID        |
 |                     |                   +--> undo/checkpoint  |
 |                     +--> health/metrics/alertas               |
 +----------------------+--------------------+-------------------+
                        |                    |
                  electrum-server      rawtx-resolver
                        |                    |
                  Electrum/Sparrow     Core -> cache -> archivo P2P
                                       (verificar antes de servir)
```

**Módulos/traits mínimos:**

- `core-adapter`: `chain_info`, `block_hash(h)`, `block_by_hash(hash)`, `block_with_prevouts(hash)` opcional, `prune_to(h)`, `set_network_active(bool)`, `broadcast_tx`, `estimate_fee`. No ocultar errores por bloque inexistente.
- `chain-supervisor`: compara tip activo; detecta reorg, lag, salud de disco y checkpoints; calcula hasta dónde **sería** seguro podar, sin asumir control sobre el algoritmo interno de Core.
- `indexer`: aplica transacciones en orden, extrae scripts, añade relaciones de historial deduplicadas, actualiza saldos y UTXO, crea datos `undo`.
- `storage`: batches atómicos, iteración prefijada, snapshots consistentes, versión/esquema, migraciones, exportación canónica.
- `merkle`: genera/verifica ramas desde TXID ordenados; no confundir `txid` con `wtxid`.
- `rawtx-resolver`: jerarquía caché → bloque retenido en Core → recuperación externa; verifica bloque, txid y testigos; tiempo máximo y errores diferenciados.
- `electrum-server`: JSON-RPC, límites/validaciones por cliente, negociación de versión y notificaciones. No exponerse públicamente en MVP.
- `mempool`: estado reconstruible, separado del confirmado, con conflictos y orden de notificaciones conforme a versión.
- `metrics`: alturas, retraso, disco, tiempo por bloque, caché, errores de prevouts, reorg, respuestas RPC; sin log de scripts consultados por defecto.

**Trust boundary:** Core valida la cadena; los eventos ZMQ son solo señales; RocksDB persiste el índice; los pares externos son adversarios potenciales. Nadie debe poder inventar un historial activo manipulando una respuesta HTTP/P2P.

## 5. Cadena confirmada y algoritmos

### 5.1 Especificación del procesamiento de un bloque

Para cada `height=h` y `blockhash=Core.getblockhash(h)`:

1. Leer bloque completo y comprobar que coincide con `blockhash` y `prev_hash` del último checkpoint. Volver a consultar la cadena activa ante un cambio de tip/reorg.
2. Asignar `tx_ref` a **cada ocurrencia de transacción**, no solo a cada TXID; conservar `(block_hash, position)` para distinguir casos históricos BIP30. En genesis aplicar tratamiento especial a la coinbase de génesis (no es gastable; véase [S04]).
3. Procesar transacciones y gastos en orden válido, resolviendo cada `vin.prevout` desde UTXO local o datos de undo de Core (si están disponibles) y registrando `(script_hash, value)` del output gastado.
4. Por cada salida no nula del bloque insertar UTXO provisional/confirmado con `txid:vout`, `value_sat`, `script_hash`, `creation_height`, `coinbase`. No almacenar solo UTXO: también registro de historial y estado de gasto reversible.
5. Construir una **relación única** `(script_hash, tx_ref)` con efecto total neto `received_sat - spent_sat` y metadatos necesarios para historia; si la tx toca un script múltiples veces se agrupa en una sola entrada histórica.
6. Conservar `txid` en **orden exacto del bloque**. Comprobar la raíz Merkle, además de identificar witness y coinbase correctamente.
7. Construir undo del bloque: entradas UTXO gastadas, salidas creadas, historial y balances modificados, índice de TXID y metadatos. No generar un `undo` incompleto por limitación de memoria.
8. Escribir **atómicamente** el bloque lógico, sus índices secundarios, el undo y `meta.committed_tip=(h,blockhash)` usando un único batch del mismo motor, con política de durabilidad documentada. Publicar disponibilidad del bloque **solo después** del commit.
9. Verificar disponibilidad/continuidad para los datos leídos; si es una cadena nueva o reorg, volver a resolver en lugar de indexar una rama obsoleta.

**Prohibido:** avanzar `committed_tip` antes del batch; resolver un prevout ausente como 0; usar `f64` para satoshis; mezclar en la BD confirmada transacciones del mempool; desduplicar históricamente una transacción solo por TXID.

### 5.2 Caso BIP30 y coinbase de génesis

BIP30 documenta TXID duplicados históricos, incluidos los bloques mainnet 91842 y 91880. El esquema `txid -> tx_ref` único es **incorrecto**. Definir `txid -> [tx_ref]` con reglas explícitas de resolución y test de estos bloques cuando se implemente mainnet. Respetar comportamiento de Core para las salidas sobrescritas por coinbases duplicadas y para la coinbase de génesis no gastable. No improvisar reglas de consenso: Core continúa siendo fuente de verdad [S09].

### 5.3 Reorganización

Ante `(Core.hash(committed_height) != db.committed_hash)`:

- Dejar de emitir respuestas que dependan de un índice incoherente (estado `REORG_RECOVERY`).
- Buscar ancestro común con la cadena activa. Revertir de arriba hacia abajo usando `undo` **durable**, en batches y checkpoints reversibles; volver a aplicar la nueva rama.
- Si no hay undo suficiente o Core ya podó los bloques necesarios, entrar en `NEEDS_RECOVERY` y requerir fuente histórica verificada/restauración; **nunca** simular que no hay operaciones históricas.
- Actualizar suscripciones relevantes y mempool después de confirmar un nuevo tip consistente. Crear prueba adversaria de reorg bajo estrés y caída durante la reversión.

### 5.4 Resistencia a fallos

- Reinicio tras `SIGKILL` entre cualquier paso: el siguiente arranque ve bloque completo o bloque ausente, jamás un checkpoint adelantado.
- Una notificación ZMQ no es confirmación de persistencia; siempre detectar huecos comprobando secuencialmente `getblockhash(h)`.
- Si el disco entra en nivel crítico o hay errores de escritura: dejar de indexar, pausar nuevas descargas en modo B cuando se pueda y emitir error **explícito**; no ejecutar poda sin `committed_tip` válido.
- Cerrar limpiamente lectura/consumidores antes de pedir poda; las llamadas RPC pueden fallar o conservar más bloques de lo solicitado.

## 6. Modos de construcción

### 6.1 Modo A — Core completo (primera implementación)

**Precondiciones:** Core sincronizado o sincronizando con archivos históricos presentes; RPC autenticado local; `prune=0`; misma red/génesis que BD; sin necesidad de `txindex`. Si Core no es realmente completo o faltan bloques históricos, reportar error preciso y ofrecer inicio limpio/restauración.

**Bucle lógico:** desde `committed_tip + 1` hasta `min(core_tip, target_height)` leer bloque por `getblockhash`/`getblock` (raw o verbosity 3 cuando proceda); indexar; verificar; persistir. En tip, modo seguimiento. Reinicios reanudan desde checkpoint. Indexación retrospectiva puede competir con IBD pero Core no poda.

**Salida verificable:** snapshot canónico de altura, hash, historial, UTXO y lista ordenada de TXID. El mismo formato será consumido por modo B.

### 6.2 Modo B — Core pruned durante IBD (segunda implementación)

**Precondiciones:** datadir nuevo con `prune=1`; no iniciar con poda automática; supervisión de disco; indexador activo desde la altura disponible más antigua, con `committed_tip` duradero; establecer capacidades RPC reales con el Core instalado. El objetivo **no** es mantener 550 MiB de bloques durante todo el IBD; se necesita margen transitorio y control de ritmo.

**Máquina de estados:** `BOOTSTRAP -> CATCH_UP -> INDEXING -> PRUNE_ELIGIBLE -> PRUNING -> CATCH_UP`; ramas `PAUSE_DOWNLOAD`, `REORG_RECOVERY`, `NEEDS_RECOVERY`, `FATAL_INTEGRITY`.

**Barrera de poda:**

```text
safe_prune_height = min(
    committed_height - undo_or_reorg_margin,
    earliest_height_still_required_by_active_readers - 1,
    core_tip - core_recent_blocks_margin
)
if safe_prune_height <= last_prune_request: do_not_prune()
else: ask_core_to_prune(safe_prune_height)
```

Esta fórmula es un **límite superior lógico conservador**, no una predicción sobre los ficheros `blk/rev` que realmente borrará Core. Los márgenes no son constantes universales, requieren configuración/experimentación, y Core aplica sus propias restricciones. No usar una altura de lector inválida, no podar si no hay snapshot duradero, y verificar el resultado RPC.

**Control de presión de disco:** al superar un high-watermark, pausar nuevas conexiones/descargas con `setnetworkactive(false)` como medida operativa *sujeta a prueba* (no detiene necesariamente lo ya recibido); acabar de indexar bloques existentes; podar hasta la altura segura y reanudar cuando exista margen real. Si el espacio libre sigue siendo insuficiente, detener el sistema de manera segura, nunca borrar archivos `blk/rev` a mano. En disco minúsculo la sincronización podría no ser viable; registrar esto como límite del producto.

**Migración a poda automática:** una vez comprobado que índice alcanza el tip, sin huecos, con backups/snapshot y pruebas de consulta antiguas satisfactorias: detener Core, cambiar `prune=1` a `prune=550`, reanudar y validar estado. Esta acción se realiza **solo por orden explícita del operador**; el software no debe modificar la configuración existente unilateralmente.

### 6.3 Test de equivalencia A/B

Construir dos bases desde **los mismos bloques activos** hasta H. Detenerse en un `blockhash` compartido y exportar contenido canónico ordenado: network/genesis, cabeceras, txids ordenados, historiales, UTXO/saldos (y undo donde alcance), excluyendo metadatos no deterministas, compacciones y mempool. SHA256 del flujo canónico debe ser idéntico. Comparar además 100+ scripts representativos y transacciones antiguas. Fallo de igualdad es **bloqueante**.

## 7. Base de datos, identidad y búsqueda

Ver contrato completo en `contracts/ALMACENAMIENTO.md`. Resumen de familias lógicas:

| Familia | Clave conceptual | Valor conceptual |
|---|---|---|
| `meta` | `format`, `network`, `genesis`, `committed_tip` | esquema/versiones/checkpoint |
| `blocks` | `(height)` | hash/cabecera/tx_count/tx_ref_base |
| `txids_by_block` | `(height,position)` | TXID de 32 bytes |
| `tx_occurrences` | `(txid, height,position)` | `tx_ref` y pertenencia activa |
| `history` | `(script_hash, height,position)` | `tx_ref, delta_sat, flags` |
| `utxo` | `(txid,vout)` | `value_sat,script_hash,created_height,coinbase` |
| `balances` | `(script_hash)` | total confirmado validable desde UTXO |
| `undo` | `(height,blockhash)` | reversión exacta de todos los efectos |
| `rawtx_cache` | `(txid, occurrence?)` | bytes, validación, tamaño/expiración |

**Tamaños de payload teóricos, NO de RocksDB:** TXID 32 B, hash de script 32 B, cabecera 80 B, referencia v1 8 B (`u64`), importe 8 B (`u64`). `5 B` para `tx_ref` comprimido es optimización FUTURA y no debe incorporarse a presupuestos v1. Un índice de búsqueda TXID exacto tiene coste propio y BIP30 exige ocurrencias.

**Endianness:** definir y probar byte order explícito para bytes del hash en disco, representación RPC invertida de scripthash en protocolo 1.4, orden lexicográfico de claves enteras en big-endian y canonicalización de TxID mostrados. Sin conversiones implícitas.

**Regla de balance:** `confirmed_balance(script)` = suma en satoshis de sus UTXO **no gastados** al mismo checkpoint, con semántica coinbase y madurez del protocolo de cliente; verificar contra historial/deltas y consultas de referencia. Para evitar lecturas a distintas alturas, cada petición usa snapshot consistente.

**Merkle:** con lista de TXID de un bloque, computar double-SHA256 por pares, duplicando último en niveles impares y comprobando raíz contra cabecera. `transaction.get_merkle(txid,height)` devuelve hermanos y posición conforme a protocolo negociado. La validación de rawtx con witness es separada: TXID solo no autentica los witness bytes.

## 8. Recuperación de transacciones antiguas y seguridad

Resolver `blockchain.transaction.get(txid)` con esta jerarquía:

1. Caché local de rawtx previamente validado.
2. Bloque activo aún disponible en Core con `getrawtransaction(txid, ..., blockhash)` cuando corresponda (o lectura de bloque completo); no asumir que `txindex=1` está habilitado.
3. Proveedor de archivo P2P/alternativa **explícita y configurable** que recupera el bloque completo de la altura/hash; validar contra cabecera de cadena activa validada por Core, hash de bloque, merkle de TXID y consistencia de compromiso witness; extraer rawtx y verificar identidad.
4. Si no hay archivo accesible, timeout o testigos no verificables: devolver error temporal distinguible; jamás responder un rawtx falsificado, `null` ambiguo o historial vacío.

`getblockfrompeer` de Core es **asíncrono**, no da undo, puede ser ignorado por pares de archivo y el bloque puede volver a podarse de inmediato. Por ello **no es un resolver fiable por sí solo**. Diseñar un componente de captura/validación, o una implementación alternativa explícita que mantenga garantías; no marcar F4 terminado antes de demostrarlo [S06].

**Privacidad:** conectar una cartera provoca consultas de script; hacer fetch de bloques específicos puede filtrar intereses. Implementar batching, caché y estrategia de consultas con privacidad documentada; WireGuard/Tor cuando corresponda. No loguear scripts/TXID consultados a nivel INFO.

## 9. Contrato de compatibilidad Electrum

Primera meta: protocolo 1.4 con `server.version` (primera petición de negociación), `server.features`, `blockchain.headers.subscribe`, `blockchain.block.header(s)`, `blockchain.scripthash.get_history`, `get_balance`, `listunspent`, `subscribe/unsubscribe` según subversión, `blockchain.transaction.get`, `get_merkle`, `id_from_pos`, `broadcast`, estimación de fees. Métodos exactos, parámetros, formatos y errores se definen en `contracts/PROTOCOLO_Y_PRUEBAS.md` y la documentación oficial [S03].

**1.7 es otra superficie de API**, no simplemente un alias: sustituye `scripthash.*` por `scriptpubkey.*`, cambia algunos formatos y notificaciones; implementar adaptador versionado y tests diferenciados [S05]. No anunciar 1.7 hasta pasar su suite.

**Readiness:** estado servidor `building_index`, `ready`, `degraded_rawtx`, `recovering`, `fatal`. En modo building no devolver vacío cuando aún no se indexó el pasado. En modo `degraded_rawtx` puede proporcionar historial válido y devolver errores sobre bytes antiguos, sin ocultarlo al operador.

## 10. Despliegue: seguridad y administración

- Entorno de laboratorio separado: `bitcoind -regtest` en datadir dedicado, sin conexión a fondos reales; después Signet, finalmente mainnet experimental. **Nunca** ejecutar comandos sobre un Core ya instalado en producción por detección automática de rutas.
- RPC Bitcoin Core por cookie, solo loopback, TLS externo no sustituye la protección de RPC. REST habilitado solo si indispensable y solo localhost (sin autenticación propia).
- Electrum listening inicialmente `127.0.0.1`, acceso remoto opcional por WireGuard/Tor/TLS tras revisión de seguridad; aplicar límites de peticiones, memoria, número de clientes y duración de respuesta.
- Usuario de sistema dedicado, permisos estrictos (`0700` directorios de BD/caché), backups atómicos, comprobación de restauración, trazas sin datos financieros innecesarios, no root, logs rotados.
- Contratos de `config.example.toml` son **interfaces planeadas**: no confundirlas con opciones ya implementadas. Documentar la versión requerida de Core como una matriz comprobada, no asumir que todas las llamadas existen en la instalación del usuario.

## 11. Rendimiento y presupuesto de almacenamiento

**Modelo general:** `N` ocurrencias de transacciones, `E` relaciones únicas `(script,tx_occurrence)`, `U` UTXO vigentes, `S` scripts distintos, `B` bloques. Un límite parcial de datos brutos del diseño es `32*N` bytes solo para TXID ordenados, más `8*E` o más para referencias, `U*(outpoint + value + script + metadata)`, índices `txid -> ocurrencias`, datos de bloques/cabeceras, undo, mempool, write amplification, caché y compactaciones.

**Ejemplo hipotético, NO una medición:** `N=1.500 millones` implica `48 GB` solo en TXID (en unidades decimales). No presupuestar MVP sobre 5 bytes de `tx_ref`, compresión ideal o un `minimal perfect hash` que aún no exista. El tamaño de RocksDB debe medirse con `properties`, `du`, WAL, checkpoints y picos de compactación, junto al disco real de Bitcoin Core podado. En mainnet no garantizar que SSD de 64/128 GB sea suficiente; reservar laboratorios con abundante margen (p. ej., 512 GB) hasta medir.

**Benchmark obligatorio:** datos de regtest heterogéneos + Signet, tamaños a 10k/100k/1M TX, extrapolación con intervalo, pico de RAM, SSD vs HDD, bytes por relación, duración/CPU de indexación, p95/p99 de consultas, latencia de rawtx cache frío y tasa de fallos de P2P. Comparar con electrs 0.12.0 y `bindex` en entorno equivalente [S07].

## 12. Estrategia de tests y puertas de salida

Implementar desde el principio tests **unitarios**, **de propiedad**, **integración regtest**, **fault injection** y **comparativos**. Casos obligatorios:

- UTXO y script hash: P2PKH, P2SH, P2WPKH, P2WSH, Taproot, OP_RETURN, scripts repetidos, salidas múltiples, coinbase, gastos y cambio; genesis coinbase; BIP30.
- Merkle: bloque 1 TX, 2 TX, 3 TX, grandes, identificación por posición; testigo SegWit inválido rechazado en recuperación externa.
- Consistencia: checkpoint atómico; desconectar Core; matar proceso en cada paso; idempotencia; compactación; backup/restauración; prueba de colisión de clave y overflow.
- IBD/poda: Core más rápido que indexador, presión de disco, poda con lectores activos, `setnetworkactive`, prune RPC error, recovery, no pérdida de bloques.
- Reorg: cambiar cadena activa 1/2/N bloques, recuperar después de crash durante undo; profundidad superior al journal disponible provoca `NEEDS_RECOVERY`.
- RPC: negociación 1.4, handshake, errores, paginación/bloques grandes, flujos de subscribe y reconexión, clients Electrum y Sparrow.
- Equivalencia: snapshot lógico A/B a mismo tip y muestras reproducibles de carteras históricas desconocidas.
- Seguridad: inputs JSON malformados, límites, rate limiting, rutas fuera del datadir bloqueadas, no publicar secreto RPC, respuestas de P2P maliciosas y oversize.

**Puertas (no avanzar sin evidencia):**

| Gate | Evidencia imprescindible |
|---|---|
| G0 → F1 | Schema, serialización, fixtures, test sin Core |
| G1 → F2 | Modo A en regtest: historia/UTXO exactos, reanuda tras crash |
| G2 → F3 | Merkle/headers y reorg correctos, RPC base validado |
| G3 → F4 | Modo B equivalente a A, disco controlado, ninguna poda de registros no confirmados |
| G4 → F5 | Rawtx antiguo incluido witness validado, errores seguros sin archivo |
| G5 → beta | Electrum y Sparrow, mempool, backups, benchmarks y revisión de seguridad |
| Beta → producción | Auditoría, pruebas adversarias, límites operativos y documentación de recuperación |

## 13. Árbol de código objetivo (NO creado todavía)

```text
electrum-lite/
  Cargo.toml
  crates/
    el-core-rpc/       # blockchain RPC/REST adapter
    el-chain/          # supervisor, modos, reorg
    el-index/          # prevouts, historial y UTXO
    el-store/          # RocksDB, esquema, migraciones, snapshot
    el-merkle/         # pruebas de inclusion
    el-rawtx/          # cache y recuperación verificada
    el-electrum/       # JSON-RPC, version negotiation
    el-mempool/        # overlay y subscriptions
    el-metrics/        # salud y alertas
  bin/electrum-lited/   # CLI + daemon
  tests/{fixtures,regtest,signet}/
  benchmarks/
  docs/                 # archivos de este paquete
  config.example.toml
  README.md
```

No crear dependencias cíclicas: `el-electrum` usa traits de lectura; no debe modificar directamente RocksDB. Los adaptadores de fuente inyectan bloques en un indexador agnóstico al modo. Interfaces públicas versionadas y pruebas de `serde`/binary codecs.

## 14. Primer entregable de programación: F0 / EL-001

**Objetivo:** demostrar que podemos representar y consultar los datos de 100.000 transacciones sintéticas y de bloques regtest sin corrupción y con una serialización estable. No comenzar construyendo el servidor público.

**Trabajo a realizar por el agente:**

1. Crear workspace Rust con `el-store`, `el-index`, `el-merkle` y test harness.
2. Fijar contratos de `NetworkIdentity`, `BlockPosition`, `TxRef`, `ScriptHash`, `Outpoint`, `UtxoValue`, `IndexedTip`, `HistoryEffect` y `UndoRecord`.
3. Probar endian/serialización, versionado y orden lexicográfico de claves; incluir outpoints BIP30, saldos con valores límite y scripts con cero/múltiples relaciones.
4. Implementar BD `Storage` provisional, preferiblemente RocksDB, con `WriteBatch` + metadata/checkpoint atómico y snapshots; probar crash/reopen.
5. Construir generador sintético de bloques y carga de 100k transacciones, medir bytes de BD, memoria, operaciones por segundo y espacio de WAL/compacción.
6. Publicar `docs/benchmarks/F0.md`, resultados reproducibles, limitaciones, versión de Rust/Core y evidencia de tests.
7. Marcar EL-001 completado **solo** si `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` y las pruebas de codec pasan en entorno documentado; si el entorno no permite un comando, anotar limitación con exactitud.

**No incluido en F0:** indexación de mainnet, login con carteras, servidor Electrum escuchando, recuperación P2P ni poda. Son fases posteriores.

## 15. Interfaces previstas y operativa (son propuestas, no programas existentes)

Ejemplo de configuración **objetivo** en `config/config.example.toml`, que habrá que implementar y validar. Las opciones de Core reales deben comprobarse con `bitcoind -help` / `bitcoin-cli help` de la versión presente. La aplicación futura debería ofrecer:

```text
# CLI FUTURA, NO EJECUTABLE TODAVIA
# electrum-lited init --config config.toml
# electrum-lited index --mode full-core --config config.toml
# electrum-lited index --mode pruned-ibd --config config.toml
# electrum-lited serve --config config.toml
# electrum-lited verify --target-height H --config config.toml
# electrum-lited export-snapshot --output snapshot.dat --config config.toml
# electrum-lited doctor --config config.toml
```

**No incluir comandos que borren, inicialicen o cambien el datadir de Bitcoin Core de producción en scripts que se ejecuten sin confirmación explícita.**

## 16. Plan de ejecución para el agente y comunicación

Seguir `PLAN_DE_TRABAJO.md` y el manifiesto `project.json`. En cada iteración:

1. Identificar ticket P0 desbloqueado y enumerar dependencias completadas.
2. Escribir pruebas para sus criterios de aceptación (rojo), luego código mínimo para pasarlas (verde), y refactorizar sin alterar contrato.
3. Ejecutar verificación local disponible, guardar comandos y salidas de referencia resumidas.
4. Actualizar documentación, decisiones y estado del backlog; registrar bloqueos con acción para resolverlos.
5. Presentar informe corto: **código cambiado, pruebas, métricas, riesgos abiertos, próximo ticket**. No declarar el producto terminado antes de G5/beta.

## 17. Riesgos no resueltos que exigen experimentos

| Riesgo | Impacto | Plan de mitigación / experimento |
|---|---|---|
| Poda automática durante IBD | Bloque perdido antes de indexar | No usar en modo B v1; `prune=1` y barrera duradera |
| Disco lleno por IBD | Corrupción/parada de Core/BD | Quota + pausa P2P + umbrales + reinicios con margen |
| Bloque histórico no disponible P2P | Falla `transaction.get` | Caché + proveedores configurables + error explícito; medir probabilidad |
| Witness externo no verificable por TXID | Respuesta engañosa | Obtener bloque completo y comprobar compromisos witness, no solo TXID |
| BIP30 / colisión de TXID histórico | Indexado falso | Índice por ocurrencia y prueba concreta con bloques 91842/91880 |
| Reorg profunda tras poda | No se puede revertir | Journal y recuperación desde snapshot/bloques históricos; degradación explícita |
| Coste real del índice | SSD insuficiente | Benchmarks antes de fijar hardware mínimo |
| Compatibilidad 1.7 | Respuestas incorrectas por formato | Versión 1.4 primero, adaptadores y tests por versión |
| Lecturas parcialmente confirmadas | Saldos/historial incoherentes | Snapshots consistentes al mismo committed tip |
| Exposición RPC/REST | Fuga/compromiso | Localhost, cookie, ACL, permisos, no root |

## 18. Fuentes primarias y puntos de verificación (revisadas 2026-11-01)

- [S01] Bitcoin Core 32 — `pruneblockchain`: https://bitcoincore.org/en/doc/32.0.0/rpc/blockchain/pruneblockchain/
- [S02] Core 0.14 — `prune=1`: https://github.com/bitcoin/bitcoin/blob/master/doc/release-notes/release-notes-0.14.0.md
- [S03] Electrum Protocol — métodos: https://electrum-protocol.readthedocs.io/en/latest/protocol-methods.html
- [S04] Electrum Protocol — `scripthash`, génesis y negociaciones: https://electrum-protocol.readthedocs.io/en/latest/protocol-basics.html
- [S05] Electrum Protocol — incompatibilidades 1.7: https://electrum-protocol.readthedocs.io/en/latest/protocol-changes.html
- [S06] Bitcoin Core 32 — `getblockfrompeer`: https://bitcoincore.org/en/doc/32.0.0/rpc/blockchain/getblockfrompeer/
- [S07] electrs 0.12 — release notes / `bindex`: https://github.com/romanz/electrs/blob/master/RELEASE-NOTES.md
- [S08] Bitcoin Core 32 — `getblock` verbosity 3: https://bitcoincore.org/en/doc/32.0.0/rpc/blockchain/getblock/
- [S09] BIP30 oficial — TXID duplicados históricos: https://github.com/bitcoin/bips/blob/master/bip-0030.mediawiki
- [S10] Bitcoin Core 32 — `getblockchaininfo`: https://bitcoincore.org/en/doc/32.0.0/rpc/blockchain/getblockchaininfo/
- [S11] Bitcoin Core — seguridad de RPC: https://github.com/bitcoin/bitcoin/blob/master/doc/JSON-RPC-interface.md
- [S12] bwt — precedente de seguimiento de carteras: https://github.com/bwt-dev/bwt
- [S13] electrs-pruned — investigación experimental: https://github.com/paulscode/electrs-pruned
- [S14] electrs — límites y requisitos 0.12: https://github.com/romanz/electrs/blob/master/doc/upgrading.md
- [S15] Bitcoin Core — REST: https://github.com/bitcoin/bitcoin/blob/master/doc/REST-interface.md

> **Revisión de fuentes:** estas URLs son puntos de partida técnicos, no sustituyen a los tests de la versión instalada. Algunas rutas `master` pueden cambiar. Las estimaciones de capacidad, rendimiento y latencia **no están validadas** para una aplicación Electrum-Lite que aún no existe.

## 19. Definición exacta de “proyecto terminado”

El producto se puede etiquetar `v1.0` únicamente cuando: ambos modos funcionan en mainnet de laboratorio, A/B igualan un estado confirmado canónico, cualquier script de prueba desconocido recupera historial y saldo, `rawtx`/Merkle/SegWit cumplen integridad, cliente Electrum y Sparrow verifican las respuestas, se gestionan reorg/crash/prune sin pérdida silenciosa, existe monitorización, backup/restore, auditoría de seguridad y se publica medición real de capacidad/discos. Si no hay peers de archivo disponibles, la documentación debe describir claramente la degradación de `transaction.get`.

**Estado al entregar este paquete:** únicamente documentación lista para guiar a una IA. No se ha creado una app funcional ni se ha ejecutado ningún test de integración de Bitcoin Core.
