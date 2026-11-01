# AGENTS.md — Protocolo operativo para agentes de código

**Misión:** implementar progresivamente Electrum-Lite Universal de acuerdo con `ESPECIFICACION_MAESTRA_IA.md`. **Estado de partida: diseño; no existe todavía aplicación.**

## Orden de lectura

1. `ESPECIFICACION_MAESTRA_IA.md` — requisitos, invariantes, arquitectura, límites y fuentes.
2. `contracts/ALMACENAMIENTO.md` — esquema lógico y semántica de escritura.
3. `contracts/PROTOCOLO_Y_PRUEBAS.md` — API y tests.
4. `PLAN_DE_TRABAJO.md` — ejecución y dependencias.
5. `project.json` — identificadores legibles por máquinas y estado de los tickets.

## Reglas imperativas

- Implementar el **primer ticket no terminado cuyas dependencias estén satisfechas**. No avanzar de fase sin su gate.
- Cada iteración debe contener pruebas unitarias/propiedad e integración cuando corresponda. Reportar comando, resultado y ruta de prueba. No inventar salidas.
- Nunca usar un datadir Bitcoin existente ni borrar archivos, reiniciar servicios o cambiar `bitcoin.conf` sin consentimiento explícito. Regtest aislado y rutas temporales por defecto.
- No cambiar formato de BD sin incremento de `schema_version`, prueba de migración o estrategia explícita de reconstrucción.
- `tx_ref` es u64 en v1; el identificador de ocurrencia incluye bloque y posición. **Nunca indexar solo por TXID** (BIP30).
- No confiar en prevouts faltantes; errores graves detienen indexación. Prohibido `unwrap` sobre entradas externas sin invariantes comprobadas; evitar floats para satoshis.
- Nunca afirmar que la validación de TXID también valida los bytes witness. No servir datos recuperados de fuente no confiable sin validación.
- No escuchar fuera de `127.0.0.1` por defecto. RPC Core con cookie y permiso mínimo. No introducir claves privadas.
- Persistir `committed_tip` y actualizaciones atómicamente y mantener `undo`. Reorg profunda no recuperable -> error claro, no historial falso.
- La poda manual solo la autoriza el componente `prune-gate` después de commit y comprobación de margen; si hay dudas, no podar.
- Versionar dependencias y registrar hashes/versions usados; no prometer compatibilidad sin comprobar el binario de Core en ejecución.
- Priorizar exactitud y reproducibilidad sobre velocidad, tamaño o número de features.

## Ciclo de trabajo por ticket

1. Escribir `docs/decisions/ADR-NNN.md` si cambia una decisión.
2. Agregar tests que fallen con el comportamiento ausente.
3. Implementar el cambio mínimo y pasar tests.
4. Ejecutar, según aplique: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, integración regtest y benchmarks.
5. Actualizar `PLAN_DE_TRABAJO.md` y `project.json` solo si la evidencia lo permite.
6. Responder en español con: ticket, archivos, tests, métricas, fallos conocidos, próximo ticket. Nunca escribir “listo” sin resultados.

## Política ante bloqueos

Falta de binario Core, permisos, conexiones P2P o dependencia externa: crear mocks/test doubles explícitos, ejecutar tests que sí sean válidos, marcar `blocked` o `partial` y describir lo que falta. No sustituir un test fallido por un stub que siempre devuelve `true`.

## Cambios de alcance

Si un requisito es imposible o incompatible con Core, documentar evidencia reproducible, alternativas, coste y recomendación. Solicitar decisión solo si afecta privacidad, seguridad, fondos, licencias o rompe un contrato aceptado. No ejecutar acciones irreversibles.

## Declaración del primer sprint

Iniciar por **EL-001**: serialización y esquema de BD con pruebas. No empezar por UI, despliegue público ni indexación de mainnet.
