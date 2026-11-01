# Paquete de especificación para IA — Electrum-Lite Universal

**Contenido:** documentación de ingeniería diseñada para ser consumida por agentes programadores como Codex/Claude Code/otras IA con acceso a un repositorio, terminal y tests. **NO contiene aún código funcional ni una app instalable.**

## Archivo principal

Leer primero [`ESPECIFICACION_MAESTRA_IA.md`](ESPECIFICACION_MAESTRA_IA.md). Es el documento autoritativo del proyecto.

## Archivos

- `AGENTS.md`: reglas que el agente debe obedecer.
- `PLAN_DE_TRABAJO.md`: tickets y puertas de salida ordenadas.
- `contracts/ALMACENAMIENTO.md`: contrato lógico del índice y codec.
- `contracts/PROTOCOLO_Y_PRUEBAS.md`: métodos Electrum y casos de prueba.
- `config/config.example.toml`: ejemplo de configuración futura (no ejecutable).
- `project.json`: backlog y estado inicial legibles por software.
- `PROMPT_PARA_IA.md`: petición inicial para pegar en la IA programadora.

## Cómo utilizarlo

1. Crear un repositorio vacío de trabajo, sin claves privadas ni datos de Core existentes.
2. Copiar el contenido íntegro de este paquete a la raíz del repositorio.
3. Dar a la IA programadora acceso a ese repositorio y a un entorno Rust/regtest **aislado**.
4. Pegar el contenido de `PROMPT_PARA_IA.md` y solicitar que ejecute **EL-001**.
5. Continuar por `PLAN_DE_TRABAJO.md`, validando pruebas y aceptaciones antes de cada etapa.

Se preserva como referencia humana el DOCX previo `Electrum-Lite_Universal_Especificacion_Tecnica_v0.1.docx`, pero los nuevos archivos Markdown son la fuente de verdad para la IA.
