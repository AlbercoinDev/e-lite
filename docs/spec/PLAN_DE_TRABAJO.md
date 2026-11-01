# Plan de trabajo por tickets — Electrum-Lite Universal

**Estado inicial de TODOS los tickets:** `todo`. No hay código funcional creado con este paquete. Este plan está diseñado para que un agente lo ejecute por iteraciones, con verificación real. Los gates no son cronogramas: describen resultados, no fechas prometidas.

## Gate G0 — Contrato y estructura (F0)

### EL-001 · Esquema lógico, codecs y BD transaccional — P0

**Dependencias:** ninguna. **Salida:** workspace Rust inicial, tipos fuertes (NetworkIdentity, BlockPosition, TxRef u64, ScriptHash, Outpoint, IndexedTip), codec versionado, Storage trait y backend provisional RocksDB; tests golden de endian, clave ordenada, identidad de red, batch atómico, reopen, undo corrupto. **Aceptación:** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`; crashes simulados no avanzan checkpoint parcial. Documentar tamaños observados o que aún faltan mediciones. **No** incluir IBD ni servicio.

### EL-002 · Fixtures sintéticos y oráculo independiente — P0

**Dependencias:** EL-001. **Salida:** generador determinista de cadena de prueba con 1k/100k TX y scripts repetidos; oráculo simple independiente de RocksDB para saldos, UTXO e historial; seeds reproducibles. **Aceptación:** paridad en corpus de 100k, sin desbordamientos, cobertura de scripts vacíos, múltiples salidas y cambio.

### EL-003 · Benchmark de almacenamiento F0 — P0

**Dependencias:** EL-001, EL-002. **Salida:** script reproducible con datos de host/SSD/compresor, GB reales de SST+WAL, pico de RAM, operaciones por segundo, ratios e intervalos. **Aceptación:** `docs/benchmarks/F0.md` con resultados y limitaciones verificables. **Gate G0**: contratos y codec correctos.

## Gate G1 — Indexación de Core completo (F1)

### EL-004 · Adaptador Bitcoin Core por RPC autenticado — P0

**Dependencias:** EL-001. **Salida:** interfaz `CoreSource`, autenticación cookie, timeouts/retry seguro, tip/hash por altura, `getblock`, capacidades RPC, pin de red/génesis; tests con Core en regtest si existe. **Aceptación:** detectar datadir incorrecto, prune inesperado, falta de bloque y reorg sin datos falsos.

### EL-005 · Extracción de scripts, prevouts y UTXO — P0

**Dependencias:** EL-001, EL-002, EL-004. **Salida:** indexador transacción/bloque en orden, resolución de prevouts con UTXO local + optimización opcional Core, relaciones `(script, tx_ref)` deduplicadas, `utxo` y balances. **Aceptación:** fixtures P2PKH/SegWit/Taproot/OP_RETURN y gasto múltiple; nadie entra en historia con `prevout=0` inventado.

### EL-006 · Aplicación atómica de bloques y reanudación — P0

**Dependencias:** EL-005. **Salida:** `committed_tip`, WAL/durabilidad definidos, undo completo, reaplicación idempotente y failpoints antes/después de commit. **Aceptación:** prueba SIGKILL/reopen en 100+ puntos; mismo fingerprint que oráculo.

### EL-007 · Modo full-core desde génesis a tip — P0

**Dependencias:** EL-004, EL-006. **Salida:** CLI inicial `index --mode full-core`, checkpoint/recovery, `getblockhash(h)` y lectura raw/verbose sin txindex obligatorio; progreso/errores claros. **Aceptación:** regtest de 1k bloques y reinicio a mitad equivalen a ejecución continua; no leer datadir no autorizado. **Gate G1.**

## Gate G2 — Consenso de datos y servidor básico (F2)

### EL-008 · Histórico BIP30, génesis y canonicidad — P0

**Dependencias:** EL-005, EL-007. **Salida:** `txid -> [ocurrencias]`, protección contra claves sobrescritas, excepción genesis coinbase, fixtures mainnet BIP30. **Aceptación:** test explícito de bloques 91842 y 91880 (fixtures validados), sin saldo duplicado.

### EL-009 · Pruebas Merkle y cabeceras — P0

**Dependencias:** EL-007. **Salida:** TXID ordenados por bloque, merkle double-SHA256, duplicación de hojas impares, proof por `(height,pos)`, block header bytes. **Aceptación:** vectores golden comparables a Core y test de transacción confirmada con inclusión válida.

### EL-010 · Servidor Electrum JSON-RPC 1.4 mínimo — P0

**Dependencias:** EL-007, EL-009. **Salida:** `server.version`, `features`, headers, scripthash.get_history/get_balance/listunspent; loopback, límites por cliente, snapshots de lectura. **Aceptación:** fixtures JSON correctos, handshake negociado, usuario puede consultar script sin registrar, `BUILDING_INDEX` no responde vacío engañoso.

### EL-011 · Reorganizaciones y journal de reversión — P0

**Dependencias:** EL-006, EL-010. **Salida:** detecta fork, rebobina UTXO/history/txids con undo, repite nueva rama y actualiza respuestas; estados de fallo recuperables. **Aceptación:** reorg de 1 y N bloques y crash en mitad de undo; sin inconsistencias. **Gate G2.**

## Gate G3 — IBD + poda manual (F3)

### EL-012 · Supervisor de disco y backpressure — P0

**Dependencias:** EL-007, EL-011. **Salida:** disco bajo/sobre umbrales, Core network active, histeresis, límites de lag y pausa/reinicio, métricas; no borrar bloques directamente. **Aceptación:** pruebas con cuota pequeña, no commits corruptos, estado claro si no hay espacio.

### EL-013 · Barrera segura de poda manual — P0

**Dependencias:** EL-012. **Salida:** Core `prune=1`, cálculo de altura segura, lectores activos y margen undo/reorg, RPC `pruneblockchain`, observabilidad. **Aceptación:** no solicitar poda por encima de altura duradera; rechazar si red errónea, skip o reader pending.

### EL-014 · Modo pruned-IBD desde cero — P0

**Dependencias:** EL-013. **Salida:** flujo de indexación y poda manual continuos desde Core vacío, recuperación de apagados y normalización de lag. **Aceptación:** Regtest/Signet de extremo a extremo, pruebas de core adelantado; ninguna pérdida silenciosa de bloques.

### EL-015 · Exportación canónica y prueba diferencial A/B — P0

**Dependencias:** EL-007, EL-014. **Salida:** exportación ordenada por red/altura/familia y hash canónico; comparador de dos DB; sin mempool ni timestamps; tipos versionados. **Aceptación:** mismo hash de snapshot en modo A y modo B sobre la **misma cadena** y muestra de UTXO/historial. **Gate G3.**

## Gate G4 — Rawtx histórico completo y confianza (F4)

### EL-016 · Resolver de rawtx desde Core y caché — P0

**Dependencias:** EL-009, EL-010. **Salida:** lectura Core por hash de bloque, caché con límites y validación de identidad, `transaction.get` con errores separados. **Aceptación:** raw correcto mientras bloque existe, timeout no rompe historial.

### EL-017 · Recuperación de bloque histórico desde archivo — P0

**Dependencias:** EL-016. **Salida:** proveedor P2P/configurable que captura bloque histórico, verifica bloque/cabecera/Merkle/witness y atiende consulta antigua; no depender exclusivamente de RPC asíncrono `getblockfrompeer`. **Aceptación:** transacción con witness en bloque podado recuperada y verificada, falsificación rechazada.

### EL-018 · Merkle RPC, id_from_pos y resiliencia rawtx — P0

**Dependencias:** EL-017. **Salida:** `transaction.get_merkle`, `transaction.id_from_pos` y errores coherentes con protocolo; caché frío, servidor de archivo offline y fuentes malas. **Aceptación:** Electrum verifica transacciones antiguas; errores cuando no haya archivo, no resultados inventados. **Gate G4.**

## Gate G5 — Integración y beta (F5)

### EL-019 · Overlay de mempool y notificaciones — P1

**Dependencias:** EL-010, EL-011. **Salida:** mempool reconstruible, subscribe/unsubscribe, secuencia/reorg coherentes, saldo confirmado/unconfirmed. **Aceptación:** emisión de TX y recepción de eventos correctos bajo retry/reinicio.

### EL-020 · Fees, broadcast y funcionalidades de servidor — P1

**Dependencias:** EL-019. **Salida:** `transaction.broadcast`, `estimatefee`, métodos auxiliares 1.4 necesarios para clientes y manejo de errores Core. **Aceptación:** fixtures + cliente compatible, sin anunciar métodos no implementados.

### EL-021 · Backup, restore y diagnósticos de operador — P1

**Dependencias:** EL-015. **Salida:** snapshot, versión de esquema, comando doctor, salud de Core/BD, backup consistente, restauración testada. **Aceptación:** restauración de copia produce idéntico fingerprint y consulta.

### EL-022 · Hardening, documentación Debian y systemd — P1

**Dependencias:** EL-020, EL-021. **Salida:** servicio no-root, localhost, cookie Core, límites, métricas, logs privados, parada limpia; despliegue Signet con datadir nuevo. **Aceptación:** análisis de superficies y pruebas de exposición de puertos.

### EL-023 · Compatibilidad Electrum y Sparrow real — P1

**Dependencias:** EL-018, EL-019, EL-020. **Salida:** tests de versiones conocidas, restauración wallet de historial antiguo y comprobación pruebas SPV; fallos documentados. **Aceptación:** comportamiento completo en escenario Signet y mainnet experimental sin desactivar comprobaciones.

### EL-024 · Benchmark mainnet y criterios de hardware — P1

**Dependencias:** EL-022, EL-023. **Salida:** consumo de RAM, discos, pico IBD, throughput, p95/p99, read amplification, comparador electrs, límites recomendados. **Aceptación:** datos medidos y reproducibles; no confundir estimación con resultado. **Gate G5.**

## Gate G6 — Mejoras opcionales (F6)

### EL-025 · Adaptador Electrum 1.7 — P2

**Dependencias:** EL-023. **Salida:** `scriptpubkey.*`, nuevos formatos, notificaciones y negociación 1.7. **Aceptación:** suite 1.7 completa sin regresiones en 1.4.

### EL-026 · Índice y compresión optimizados — P2

**Dependencias:** EL-024. **Salida:** comparar layout de RocksDB, bindex, compresión de historial, compact lookup, 40-bit refs solo con prueba de migración. **Aceptación:** menor coste real sin perder corrección y benchmarks antes/después.

### EL-027 · Reducción del mínimo de hardware — P2

**Dependencias:** EL-024, EL-026. **Salida:** perfiles medidos de 2/4/8 GiB RAM, HDD/SSD, discos de 128/256/512 GB. **Aceptación:** requisitos basados en medidas, no supuestos.

---

## Formato de informe obligatorio en cada iteración

```text
Ticket: EL-XXX
Estado: done | partial | blocked
Archivos creados/modificados:
Pruebas nuevas:
Comandos realmente ejecutados + resultado:
Benchmarks (si aplica):
Riesgos/bugs pendientes:
Cambios de contrato/ADR:
Próximo ticket desbloqueado:
```

**Prioridad de la primera iteración:** EL-001, luego EL-002, EL-003; avanzar a indexador Core completo, después poda manual, y no al revés. El archivo `project.json` es el manifiesto estructurado de estado; inicialmente todo es `todo`.
