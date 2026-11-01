# Contrato lógico de almacenamiento v1 (borrador implementable)

**Documento subordinado a** `ESPECIFICACION_MAESTRA_IA.md`. Este esquema es un contrato de intención y **requiere concretar un codec exacto en EL-001**; no afirmar compatibilidad binaria hasta añadir golden fixtures y versionado. Evitar confundir tamaño de campos con tamaño de RocksDB.

## 1. Tipos de identidad

```text
NetworkIdentity { chain_name: string, genesis_hash: [u8;32], signet_challenge_hash?: [u8;32] }
BlockPosition   { block_hash: [u8;32], height: u32, tx_pos: u32 }
TxRef           = u64  // id de ocurrencia; no es una identidad por TXID
ScriptHash      = sha256(scriptPubKey_raw_bytes)  // 32 bytes canónicos
Outpoint        { txid: [u8;32], vout: u32 }
UtxoValue       { sat: u64, script_hash: [u8;32], creation_height: u32, coinbase: bool }
IndexedTip      { height: u32, block_hash: [u8;32], schema_version: u32 }
HistoryEffect   { tx_ref: u64, received_sat: u64, spent_sat: u64, flags: u16 }
```

Todos los hashes tienen representaciones **separadas** (`HashBytes` vs `DisplayHex`). No comparar directamente el hexadecimal de scripthash Electrum 1.4 con bytes SHA-256 internos: su representación se invierte. Prohibida la mezcla implícita de redes y la saturación silenciosa de enteros.

## 2. Familias lógicas y claves propuestas

| Familia | Clave lógica | Valor | Invariante |
|---|---|---|---|
| `meta` | `format_version` | u32 | Rechazar schema más nuevo |
| `meta` | `genesis_hash`, `network`, `committed_tip` | identidad y checkpoint | No abrir BD de otra red |
| `blocks` | `height` | blockhash, header[80], tx_count, tx_ref_base? | Una rama activa por altura |
| `txids_by_block` | `(height, tx_position)` | txid[32] | Orden dentro del bloque |
| `tx_occurrences` | `(txid, height, tx_position)` | tx_ref | Permite BIP30 |
| `txref_position` | `tx_ref` | `(height, tx_position, blockhash)` | Búsqueda por ocurrencia |
| `history` | `(script_hash, height, tx_position)` | tx_ref + `received_sat` + `spent_sat` | Una fila por script/ocurrencia |
| `utxo` | `(txid, vout)` | UtxoValue | Representa UTXO de cadena activa |
| `balances` | `script_hash` | u64 sat | Debe igualar suma de UTXO |
| `undo` | `(height, blockhash)` | operaciones reversibles + checksums | Revertir bloque completamente |
| `rawtx_cache` | `(txid, blockhash?)` | bytes + validación + cuota | No cachear raw no verificado |
| `checkpoint_log` | `(height)` | blockhash/schema/version/canonical digest opcional | Auditoría/diagnóstico |

**N.B.** Un `outpoint` histórico duplicado puede requerir tratamiento excepcional BIP30 al aplicar bloques de mainnet. No interpretar esta tabla como implementación completa de ese caso: fixture real obligatorio antes de mainnet. Mantener el contexto de bloque en índices históricos para no descartar ocurrencias; el UTXO representa solo salidas vigentes y sigue la semántica de Core.

## 3. Contrato de serialización en EL-001

- Claves de enteros con **big-endian de longitud fija** para preservar orden lexicográfico. Valores con orden explícito, no `bincode` sin formato versionado documentado.
- Claves prefijadas por `family_id` y longitud fija o componentes con `length-prefix` validado; preferir Column Families cuando simplifiquen iteración.
- Header de `undo`: magic, `schema_version`, red/hash/altura, longitud límite, checksum; si payload está corrompido, abortar reversión segura.
- Serialización determinista para snapshot, independiente de compacción de RocksDB. Ordenación por familias + claves canónicas, sin timestamps ni cachés ni metadatos de host.
- Política de límites y ataques: valores máximos para scripts, transacciones, número de outputs, tamaño de bloque, records de history, JSON-RPC y cache. Obtener máximos reales de reglas Core/protocolo; no inventarlos.

## 4. Atomicidad y transacciones

```text
apply_block(block):
  assert block.previous_hash == meta.committed_tip.hash
  assert all_prevouts_resolved_or_verified()
  changes, undo = calculate_changes(block)
  assert merkle_root(txids_by_block(block)) == block.header.merkle_root
  batch = storage.new_batch()
  batch.put(all changes to blocks/txids/lookup/history/utxo/balances)
  batch.put(undo(block))
  batch.put(meta.committed_tip, {height,hash})
  storage.commit_durable(batch)  # definir WAL + sync policy
  publish_new_tip(height,hash)
```

Los snapshots de lectura deben fijar una altura de cadena coherente; el consumidor Electrum nunca debe mezclar un historial del bloque H y UTXO del H+1 sin indicarlo.

## 5. Tamaño mínimo teórico y benchmark

`txid[32]`, `script_hash[32]`, `tx_ref[u64]`, `value[u64]`, `header[80]`; esto **no** implica registro de 32/8/80 bytes en RocksDB. Medir `on_disk_live_data_size`, `total_sst`, `WAL`, caché, compacción, peak IBD y read/write amplification. El primer prototipo NO presupone compresión de 5 bytes ni un `minimal perfect hash` aún no implementado.

## 6. Verificación del codec

- Golden vectors de hash, bytes, endian y script hash reversed.
- Ronda encode/decode de datos vacíos y máximos.
- Round trip de merkle y snapshot después de reopen.
- Rechazo de checksum inválido, versiones desconocidas, longitudes imposibles, key collision e identidad de red distinta.
- Casos explícitos BIP30, genesis coinbase, SegWit y reorg.
