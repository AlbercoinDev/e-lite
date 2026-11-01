# Contrato de protocolo Electrum y matriz de pruebas

## Protocolos y versiones

**Primera implementación:** negociar y anunciar solamente versión Electrum **1.4** compatible con las pruebas reales de clientes. No anunciar 1.7 hasta implementar separadamente los cambios de `scriptpubkey.*`, campos y notificaciones. La negociación `server.version` es la primera solicitud según especificación posterior; desarrollar compatibilidad explícita y probar clientes.

Documentación: https://electrum-protocol.readthedocs.io/en/latest/protocol-methods.html y https://electrum-protocol.readthedocs.io/en/latest/protocol-changes.html

## Métodos por prioridad

| Fase | Método | Dependencias | Prueba mínima |
|---|---|---|---|
| F2 | `server.version` | negociación | Rechazar versión incompatible sin falsos anuncios |
| F2 | `server.features` | red/cabeceras | genesis_hash y rangos correctos |
| F2 | `blockchain.headers.subscribe` | headers activas | Reorg actualiza notificación |
| F2 | `blockchain.block.header` | blocks | Cabecera exacta 80 B |
| F2 | `blockchain.block.headers` | blocks | Rango, límites y formato |
| F2 | `blockchain.scripthash.get_history` | history | Script antiguo desconocido, vacío realmente vacío |
| F2 | `blockchain.scripthash.get_balance` | UTXO/overlay | Saldo exacto por altura y mempool |
| F2 | `blockchain.scripthash.listunspent` | UTXO | Outpoint, importe, altura correctos |
| F3/F5 | `blockchain.scripthash.subscribe` | history/mempool | Estado y actualizaciones |
| F4 | `blockchain.transaction.get_merkle` | txids + headers | Branch y posición verificados |
| F4 | `blockchain.transaction.id_from_pos` | txids + merkle | Modo con/sin rama |
| F4 | `blockchain.transaction.get` | rawtx resolver | Raw exacto, witness verificado |
| F5 | `blockchain.transaction.broadcast` | Core RPC | Errores propagados sin ocultar |
| F5 | `blockchain.estimatefee` | Core RPC | Datos representativos, errores |
| F5 | `blockchain.scripthash.get_mempool` | mempool | Orden y altura correctos |
| F6 | `blockchain.scriptpubkey.*` | adaptador 1.7 | Nuevo tipo input/output + notificaciones |

El mapa de campos exacto se debe generar como fixture JSON por método a partir de la documentación de la versión negociada; no copiar respuestas de una versión a otra. Nombres y disponibilidad de métodos de soporte (ping, relayfee, mempool, etc.) se comprobarán con la matriz de clientes. Limitar tamaño de historial y responder error normalizado donde aplique, nunca truncar silenciosamente.

## Estado del índice y semántica de error

- `READY`: consultas a mismo `indexed_tip` con snapshot consistente.
- `BUILDING_INDEX`: no devolver inexistencia para alturas/scripts aún no procesados; error de servidor temporal.
- `REORG_RECOVERY`: rechazar o aplazar consultas que dependan de cadena incoherente; no mezclar dos ramas.
- `DEGRADED_RAWTX`: historial y UTXO disponibles, `transaction.get` antiguo devuelve error temporal si falló recuperación.
- `FATAL_INTEGRITY`: detener servicio hasta intervención/recuperación.

No inventar códigos JSON-RPC: seleccionar conforme a especificación de la versión y documentar cada fixture. **No confundir una respuesta vacía legítima con fallo de indexación.**

## Matriz de casos de aceptación

| ID | Caso | Evidencia |
|---|---|---|
| T-001 | 1000 bloques regtest, scripts repetidos | historia y UTXO frente a oráculo |
| T-002 | A/B mismo tip en regtest y Signet | fingerprint canónico idéntico |
| T-003 | TXID duplicado histórico BIP30 | índice conserva ocurrencias |
| T-004 | Génesis coinbase | no gasto ni saldo falso |
| T-005 | Inputs consumen varias salidas del mismo script | solo una entrada `history` por tx/script |
| T-006 | 1, 2, 3 y N TX por bloque | merkle root y branch correctos |
| T-007 | rawtx SegWit con witness adulterado | rechazo con motivo y no caché |
| T-008 | Fondo histórico y rawtx podado sin archivo disponible | error explícito, sin falso vacío |
| T-009 | Recuperación de bloque antiguo desde archivo | rawtx y witness verificables |
| T-010 | SIGKILL antes/durante/después de commit | mismo checkpoint y estado íntegro |
| T-011 | Reorg 1/N y crash durante undo | retorno a estado correcto |
| T-012 | Pruning gate con indexador lento | cero altura eliminada sin confirmar |
| T-013 | IBD sobre disco casi lleno | pausa o halt seguro, sin borrado manual |
| T-014 | ZMQ perdido/duplicado | continuidad por RPC |
| T-015 | 1.4 server.version + features | fixtures JSON + clientes reales |
| T-016 | Electrum + Sparrow restauración cartera antigua | historial/UTXO/merkle completos |
| T-017 | Búsqueda de script que nunca se registró | idéntico a indexador de referencia |
| T-018 | Corrupción de `undo` / schema incompatible | error y recuperación explícitos |
| T-019 | Peer P2P malicioso, timeout, oversized block | no servir datos inválidos |
| T-020 | Benchmarks 10k/100k/1M tx | bytes, RAM, latencia, pico de disco |

**Definición de éxito:** resultado `PASS` solo con salida reproducible almacenada. `SKIPPED` si el entorno carece de Core/binarios o conexión, nunca `PASS` por ausencia de pruebas.
