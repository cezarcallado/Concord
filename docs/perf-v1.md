# Consumo da v1.0: medição antes e depois (fase 1, 2026-10-17)

Regra número 1 do plano (docs/PLANO-v1.0.md): RAM no máximo +10% por cenário, processador igual em
repouso, placa de vídeo igual. Esta é a medição da **fase 1** (tema, fontes, ícones, componentes,
aplicados no app sem mudar as telas) contra a **v0.9.1** (o `voxhub.exe` do zip de
releases\v0.9.1, idêntico ao compilado da tag).

## Como foi medido

`tools\perf.ps1` (em modo de teste em segundo plano: nenhuma janela toma o foco, nada toca som). Cada
cenário tem o próprio hub de teste (7997, LiveKit 7970-7972). O cliente medido é sempre o mesmo
(`pfA1`); depois que a cena se acomoda, ele é amostrado uma vez por segundo durante 20 s:

- working set e bytes privados (média);
- processador: tempo de CPU do processo / tempo de relógio, em % de um núcleo;
- placa de vídeo: contadores `\GPU Engine(pid_<PID>*)\Utilization Percentage`, todos os motores
  somados e só os motores 3D (média).

Cenários:

- (a) tela inicial parada: execução normal (não autoteste) com APPDATA/LOCALAPPDATA vazios e próprios
  (nenhum servidor salvo, sem ícone na bandeja);
- (b) chamada de voz com outro cliente, o chat de um canal de texto no centro com algumas mensagens,
  nada mudando (papel `voice --show previews`, já em repouso);
- (c) assistindo uma transmissão (Go Live de uma `test_window`) dentro da sala, depois das
  verificações do papel `b` (em repouso no passo 29);
- (d) câmera própria ligada (câmera sintética, `VOX_FAKE_CAMERA=1`), com outro cliente na sala.

Duas rodadas de cada cenário em cada versão; a tabela de cima é a média das duas.

## Resultado

| Cenário | Working set (MB) v0.9.1 → fase 1 | Bytes privados (MB) v0.9.1 → fase 1 | CPU (% de 1 núcleo) | GPU, todos os motores (%) | GPU 3D (%) |
|---|---|---|---|---|---|
| (a) Tela inicial parada | 81.6 → 81.5 (-0.1%) | 80.2 → 79.9 (-0.3%) | 0.11 → 0.00 | 0.00 → 0.00 | 0.00 → 0.00 |
| (b) Chamada com chat | 20.3 → 21.0 (+3.7%) | 108.2 → 109.6 (+1.2%) | 1.61 → 1.34 | 0.00 → 0.00 | 0.00 → 0.00 |
| (c) Assistindo transmissão | 59.8 → 59.8 (+0.1%) | 170.5 → 168.3 (-1.3%) | 12.14 → 12.54 | 1.58 → 1.55 | 1.56 → 1.53 |
| (d) Câmera ligada | 43.5 → 44.0 (+1.3%) | 227.4 → 228.1 (+0.3%) | 7.75 → 7.87 | 4.31 → 4.31 | 1.98 → 1.99 |

**Dentro do orçamento.** RAM: no máximo +3,7% (working set da chamada, ~0,7 MB) e +1,2% em bytes
privados. Processador em repouso na tela inicial: 0. Placa de vídeo: igual (0 na tela inicial e na
chamada parada, porque nada redesenha). As diferenças de CPU em (b), (c) e (d) estão dentro da
variação entre rodadas da própria v0.9.1 (ex.: 11,7 a 12,6% em (c)).

Observações:

- O working set da chamada é baixo nas duas versões porque entrar na voz devolve a memória ao sistema
  (`stats::trim_memory`); os bytes privados são a medida mais estável ali.
- Em (c) e (d) os papéis de autoteste ainda pedem um quadro a cada 100 ms (igual nas duas versões), o
  que soma algum processador ao que um usuário veria.

### Rodadas

| Rodada | Cenário | WS (MB) | Privados (MB) | CPU (%) | GPU todos (%) | GPU 3D (%) |
|---|---|---|---|---|---|---|
| v0.9.1 #1 | home | 81.7 | 80.3 | 0.07 | 0 | 0 |
| v0.9.1 #2 | home | 81.5 | 80 | 0.15 | 0 | 0 |
| v0.9.1 #1 | call | 20.4 | 107 | 1.53 | 0 | 0 |
| v0.9.1 #2 | call | 20.2 | 109.5 | 1.69 | 0 | 0 |
| v0.9.1 #1 | watch | 59.7 | 170.7 | 12.56 | 1.56 | 1.53 |
| v0.9.1 #2 | watch | 59.8 | 170.3 | 11.73 | 1.6 | 1.58 |
| v0.9.1 #1 | camera | 43.3 | 229.4 | 7.29 | 4.32 | 1.99 |
| v0.9.1 #2 | camera | 43.7 | 225.5 | 8.21 | 4.3 | 1.97 |
| fase 1 #1 | home | 81.5 | 80.2 | 0 | 0 | 0 |
| fase 1 #2 | home | 81.6 | 79.6 | 0 | 0 | 0 |
| fase 1 #1 | call | 21.2 | 111.3 | 1.69 | 0 | 0 |
| fase 1 #2 | call | 20.9 | 107.9 | 1 | 0 | 0 |
| fase 1 #1 | watch | 59.7 | 167.6 | 13.28 | 1.51 | 1.49 |
| fase 1 #2 | watch | 59.9 | 169.1 | 11.8 | 1.59 | 1.57 |
| fase 1 #1 | camera | 44.1 | 229.2 | 7.82 | 4.31 | 2 |
| fase 1 #2 | camera | 44 | 226.9 | 7.91 | 4.31 | 1.98 |

## Tamanho

| | Bytes |
|---|---|
| Atkinson Hyperlegible Next Regular (instância estática) | 48 128 |
| Atkinson Hyperlegible Next SemiBold (instância estática) | 48 036 |
| Bricolage Grotesque SemiBold (instância estática) | 90 992 |
| Phosphor regular, só os 59 ícones escolhidos (o arquivo inteiro tem 488 636) | 18 788 |
| **Fontes novas no total (4 arquivos)** | **205 944** |
| `voxhub.exe` v0.9.1 → fase 1 | 69 417 984 → 69 693 952 (+275 968, +0,4%) |

## Repetir

```
$env:CARGO_TARGET_DIR = "target-v02"; cargo build --release --workspace
powershell -ExecutionPolicy Bypass -File tools\perf.ps1 -Exe <v0.9.1>\voxhub.exe -Label v0.9.1 -Out base.csv -Repeat 2
powershell -ExecutionPolicy Bypass -File tools\perf.ps1 -Label fase1 -Out fase1.csv -Repeat 2
```

## Fase 2a (barra lateral, chat, tela inicial, navegação; 2026-10-17)

Mesmo `tools\perf.ps1`, duas rodadas (`-Label fase2a -Repeat 2`), o build da árvore de trabalho com
a fase 2a (commits af677ca e 38037ac) e o palco da fase 2b como estava naquela hora. No cenário (b)
o centro mostra o chat do canal de texto (modo "só chat", a lista de membros à direita), como antes.

| Cenário | Working set (MB) v0.9.1 → fase 2a | Bytes privados (MB) v0.9.1 → fase 2a | CPU (% de 1 núcleo) | GPU, todos os motores (%) | GPU 3D (%) |
|---|---|---|---|---|---|
| (a) Tela inicial parada | 81.6 → 82.1 (+0.6%) | 80.2 → 79.4 (-1.0%) | 0.11 → 0.04 | 0.00 → 0.00 | 0.00 → 0.00 |
| (b) Chamada com chat | 20.3 → 19.7 (-3.2%) | 108.2 → 105.3 (-2.7%) | 1.61 → 1.35 | 0.00 → 0.01 | 0.00 → 0.01 |
| (c) Assistindo transmissão | 59.8 → 59.8 (-0.1%) | 170.5 → 168.8 (-1.0%) | 12.14 → 11.00 | 1.58 → 1.61 | 1.56 → 1.59 |
| (d) Câmera ligada | 43.5 → 43.6 (+0.1%) | 227.4 → 230.3 (+1.3%) | 7.75 → 7.49 | 4.31 → 4.27 | 1.98 → 1.95 |

**Dentro do orçamento**: RAM no máximo +1,3% (bytes privados com a câmera), processador em repouso 0
na tela inicial, placa de vídeo igual (0,01% na chamada parada é ruído de uma das rodadas).

| Rodada | Cenário | WS (MB) | Privados (MB) | CPU (%) | GPU todos (%) | GPU 3D (%) |
|---|---|---|---|---|---|---|
| fase 2a #1 | home | 82 | 79.7 | 0 | 0 | 0 |
| fase 2a #2 | home | 82.2 | 79.1 | 0.08 | 0 | 0 |
| fase 2a #1 | call | 19.7 | 105.6 | 1.92 | 0.01 | 0.01 |
| fase 2a #2 | call | 19.6 | 105 | 0.77 | 0 | 0 |
| fase 2a #1 | watch | 59.8 | 167.4 | 10.03 | 1.65 | 1.63 |
| fase 2a #2 | watch | 59.7 | 170.1 | 11.96 | 1.57 | 1.55 |
| fase 2a #1 | camera | 43.3 | 229.8 | 6.36 | 4.24 | 1.94 |
| fase 2a #2 | camera | 43.8 | 230.7 | 8.61 | 4.29 | 1.95 |

## Fase 2b (o palco da sala de voz; 2026-10-17)

Mesmo `tools\perf.ps1` (`-Label fase2b -Repeat 2`), build da árvore com as fases 2a e 2b (commits até
ec2d02d): cards do kit, fixar vários, ocultar cards sem vídeo, tela cheia do palco e o dock de voz no
pé do palco. No cenário (c) o palco mostra a transmissão fixada (área grande) e os cards das pessoas
na faixa de baixo; o layout só é recalculado quando muda o conjunto de cards, os fixados, os ocultos
ou o retângulo, e as janelas de vídeo só se movem quando o retângulo delas muda.

| Cenário | Working set (MB) v0.9.1 → fase 2b | Bytes privados (MB) v0.9.1 → fase 2b | CPU (% de 1 núcleo) | GPU, todos os motores (%) | GPU 3D (%) |
|---|---|---|---|---|---|
| (a) Tela inicial parada | 81.6 → 81.9 (+0.4%) | 80.2 → 79.7 (-0.6%) | 0.11 → 0.08 | 0.00 → 0.00 | 0.00 → 0.00 |
| (b) Chamada com chat | 20.3 → 20.0 (-1.6%) | 108.2 → 105.6 (-2.4%) | 1.61 → 0.86 | 0.00 → 0.01 | 0.00 → 0.01 |
| (c) Assistindo transmissão | 59.8 → 59.7 (-0.2%) | 170.5 → 168.7 (-1.1%) | 12.14 → 11.05 | 1.58 → 1.59 | 1.56 → 1.57 |
| (d) Câmera ligada | 43.5 → 43.9 (+0.8%) | 227.4 → 227.6 (+0.1%) | 7.75 → 8.42 | 4.31 → 4.28 | 1.98 → 1.95 |

**Dentro do orçamento**: RAM no máximo +0,8% (working set com a câmera), processador em repouso igual
na tela inicial, placa de vídeo igual. O cenário que mais importa, (c), ficou igual ou abaixo da
v0.9.1 (CPU 11,05% contra 12,14%; a variação entre rodadas da própria v0.9.1 é de 11,7 a 12,6%).
A CPU de (d) está dentro da variação entre rodadas (7,8 a 9,0%; a fase 2a teve 6,4 a 8,6%).

A linha (b) é a média de três rodadas feitas à parte (`-Only call -Repeat 3`). Na primeira medição
completa, a rodada #2 de (b) saiu fora da curva (CPU 22,6%, GPU 1,7%, 1096 quadros em 30 s): a
janela redesenhou sem parar com o chat no centro (o palco nem estava na tela, "camera tiles
visibility visible=false" no log). Não se repetiu em nenhuma das outras quatro rodadas de (b) nem nas
da fase 2a; fica anotado para a revisão final (fase 4).

| Rodada | Cenário | WS (MB) | Privados (MB) | CPU (%) | GPU todos (%) | GPU 3D (%) |
|---|---|---|---|---|---|---|
| fase 2b #1 | home | 82 | 80.4 | 0.07 | 0 | 0 |
| fase 2b #2 | home | 81.8 | 79 | 0.08 | 0 | 0 |
| fase 2b #1 | call | 20.1 | 104.6 | 1.46 | 0 | 0 |
| fase 2b #2 | call (fora da curva) | 36.1 | 109.5 | 22.63 | 1.68 | 1.68 |
| fase 2b #3 | call | 20 | 104.8 | 0.9 | 0.01 | 0.01 |
| fase 2b #4 | call | 19.8 | 104.8 | 1.23 | 0.01 | 0.01 |
| fase 2b #5 | call | 20.1 | 107.2 | 0.46 | 0.01 | 0.01 |
| fase 2b #1 | watch | 59.5 | 168.7 | 9.95 | 1.55 | 1.52 |
| fase 2b #2 | watch | 59.9 | 168.7 | 12.14 | 1.63 | 1.61 |
| fase 2b #1 | camera | 43.9 | 225.8 | 7.8 | 4.25 | 1.93 |
| fase 2b #2 | camera | 43.8 | 229.3 | 9.04 | 4.31 | 1.97 |

## Fase 3a (janelas secundárias: Configurações, câmera, sons, modificador de voz, Go Live; 2026-10-17)

Mesmo `tools\perf.ps1` (`-Label fase3a -Repeat 2`), build da árvore com a fase 3a inteira (commits até
4715227) e o trabalho da fase 3b como estava naquela hora (inclusive a vigia de repintura,
`repaint_watch.rs`: coluna "busy" 0 em todas as rodadas). Os cenários medem as telas de sempre; as
janelas da fase 3a ficam fechadas neles (abertas, elas só existem enquanto estão na tela: o Go Live
solta as miniaturas e as texturas ao fechar, a câmera solta a prévia).

| Cenário | Working set (MB) v0.9.1 → fase 3a | Bytes privados (MB) v0.9.1 → fase 3a | CPU (% de 1 núcleo) | GPU, todos os motores (%) | GPU 3D (%) |
|---|---|---|---|---|---|
| (a) Tela inicial parada | 81.6 → 82.2 (+0.7%) | 80.2 → 81.1 (+1.1%) | 0.11 → 0.00 | 0.00 → 0.00 | 0.00 → 0.00 |
| (b) Chamada com chat | 20.3 → 19.9 (-2.0%) | 108.2 → 104.8 (-3.1%) | 1.61 → 1.53 | 0.00 → 0.00 | 0.00 → 0.00 |
| (c) Assistindo transmissão | 59.8 → 60.4 (+1.0%) | 170.5 → 168.7 (-1.1%) | 12.14 → 13.62 | 1.58 → 1.62 | 1.56 → 1.60 |
| (d) Câmera ligada | 43.5 → 43.8 (+0.6%) | 227.4 → 226.5 (-0.4%) | 7.75 → 7.95 | 4.31 → 4.43 | 1.98 → 2.07 |

**Dentro do orçamento** de RAM (no máximo +1,1%, bytes privados na tela inicial), processador em
repouso 0 na tela inicial e na chamada parada sem repintura, placa de vídeo igual. A CPU de (c) ficou
acima da faixa da v0.9.1 (13,0 e 14,2% contra 11,7 a 12,6%); a fase 3a não toca o caminho do vídeo
(o seletor do Go Live está fechado nesse cenário), e as fases 2a/2b mediram 10,0 a 12,1% no mesmo
cenário, então fica anotado para a medição final da fase 4 junto com o resto da árvore da fase 3b.

| Rodada | Cenário | WS (MB) | Privados (MB) | CPU (%) | GPU todos (%) | GPU 3D (%) |
|---|---|---|---|---|---|---|
| fase 3a #1 | home | 81.8 | 79.3 | 0 | 0 | 0 |
| fase 3a #2 | home | 82.5 | 82.8 | 0 | 0 | 0 |
| fase 3a #1 | call | 19.7 | 105.6 | 1.61 | 0 | 0 |
| fase 3a #2 | call | 20.1 | 104 | 1.45 | 0 | 0 |
| fase 3a #1 | watch | 60.2 | 168.6 | 14.21 | 1.53 | 1.51 |
| fase 3a #2 | watch | 60.6 | 168.8 | 13.02 | 1.71 | 1.69 |
| fase 3a #1 | camera | 43.7 | 225.4 | 9.62 | 4.3 | 1.98 |
| fase 3a #2 | camera | 43.8 | 227.5 | 6.28 | 4.55 | 2.16 |
## Fase 3b (janela do servidor, menus, perfil, DMs, diálogos; 2026-10-17)

Mesmo `tools\perf.ps1` (`-Label fase3b -Repeat 2`, mais `-Only call -Repeat 5` duas vezes), build da
árvore com as fases 3a e 3b inteiras (commits até 3b82ba8). A coluna nova "busy" conta as linhas
"continuous repaint" que o cliente medido escreveu durante a medição (`repaint_watch.rs`, abaixo).

| Cenário | Working set (MB) v0.9.1 → fase 3b | Bytes privados (MB) v0.9.1 → fase 3b | CPU (% de 1 núcleo) | GPU, todos os motores (%) | GPU 3D (%) |
|---|---|---|---|---|---|
| (a) Tela inicial parada | 81.6 → 82.0 (+0.5%) | 80.2 → 79.2 (-1.2%) | 0.11 → 0.04 | 0.00 → 0.00 | 0.00 → 0.00 |
| (b) Chamada com chat | 20.3 → 19.9 (-2.0%) | 108.2 → 107.0 (-1.1%) | 1.61 → 0.85 | 0.00 → 0.01 | 0.00 → 0.01 |
| (c) Assistindo transmissão | 59.8 → 60.7 (+1.5%) | 170.5 → 169.5 (-0.6%) | 12.14 → 11.26 | 1.58 → 1.66 | 1.56 → 1.64 |
| (d) Câmera ligada | 43.5 → 43.9 (+0.9%) | 227.4 → 229.4 (+0.9%) | 7.75 → 8.68 | 4.31 → 4.43 | 1.98 → 2.08 |

**Dentro do orçamento**: RAM no máximo +1,5% (working set assistindo), processador em repouso igual
(0,04% na tela inicial), placa de vídeo igual. A linha (b) é a média das 12 rodadas da chamada (2 da
medição completa e 10 das duas séries `-Only call -Repeat 5`): CPU de 0,46 a 1,68%, nenhuma fora da
curva, e "busy" 0 em todas as que foram medidas depois do último ajuste da vigia (as anteriores
contavam, por engano, a janela calma logo depois do tique do próprio autoteste). A CPU de (c) voltou
para a faixa da v0.9.1 (10,0 e 12,5%); a de (d) está dentro da variação entre rodadas (6,3 a 9,6%
nas fases 2b e 3a).

### A repintura contínua da fase 2b

A rodada #2 de (b) da fase 2b (22,6% de CPU, 1096 quadros em 30 s, o chat no centro e o palco fora
da tela) não se repetiu em nenhuma das 12 rodadas acima, nem nas 10 de antes (fases 2a/2b/3a). Os
logs daquela rodada não existem mais, então a causa exata não pôde ser lida; o que a fase 3b fez:

- **O medidor do microfone parou de acordar a janela quando não está na tela.** A thread do medidor
  (`MeterWatch`, até 15 repinturas por segundo enquanto alguém fala) ficava ligada durante a chamada
  inteira, também com só o chat no centro, onde a fase 2b tirou o medidor (ele está no dock do
  palco). Agora ela só roda enquanto o dock desenhou o nível no quadro anterior
  (`App::meter_visible` / `CallUi::meter_frame`). Nos testes ninguém fala (sem microfone), por isso
  isso não aparece nos números acima; com um microfone de verdade, era repintura sem motivo a cada
  3 dB de nível.
- **A vigia de repintura** (`repaint_watch.rs`): conta os quadros em janelas de 2 s; com mais de 12
  por segundo sem entrada da pessoa em duas janelas seguidas, ela guarda quem pediu cada repintura
  (o arquivo e a linha de cada `request_repaint`, pelo `Context::repaint_causes` do egui) e escreve
  os maiores no log ("continuous repaint", no máximo a cada 10 s). Se o laço voltar, o log de quem
  pegar diz de onde ele vem; o `perf.ps1` mostra essas linhas na coluna "busy".
- **O teste de repouso** (autoteste `nav`, no gate): com só o chat no centro durante uma chamada e
  nada mudando, 8 s podem ter no máximo 16 quadros (mediu 8) e a thread do medidor tem de estar
  desligada.

| Rodada | Cenário | WS (MB) | Privados (MB) | CPU (%) | GPU todos (%) | GPU 3D (%) | busy |
|---|---|---|---|---|---|---|---|
| fase 3b #1 | home | 81.9 | 78.9 | 0.07 | 0 | 0 | 0 |
| fase 3b #2 | home | 82.1 | 79.5 | 0 | 0 | 0 | 0 |
| fase 3b #1 | call | 20 | 105.6 | 0.92 | 0.01 | 0.01 | (1) |
| fase 3b #2 | call | 19.6 | 107.8 | 1.68 | 0 | 0 | (1) |
| fase 3b #1 | watch | 60.7 | 168 | 10.03 | 1.75 | 1.73 | 1 |
| fase 3b #2 | watch | 60.7 | 171 | 12.48 | 1.56 | 1.54 | 1 |
| fase 3b #1 | camera | 43.9 | 231.7 | 8.73 | 4.52 | 2.15 | 2 |
| fase 3b #2 | camera | 43.8 | 227 | 8.63 | 4.33 | 2 | 2 |
| fase 3b call #1 | call | 20.5 | 109.4 | 1.27 | 0.01 | 0.01 | (1) |
| fase 3b call #2 | call | 20.1 | 105.4 | 0.61 | 0 | 0 | (1) |
| fase 3b call #3 | call | 20.1 | 106.8 | 0.92 | 0.01 | 0.01 | (1) |
| fase 3b call #4 | call | 19.6 | 110.5 | 0.92 | 0.01 | 0.01 | (1) |
| fase 3b call #5 | call | 19.9 | 106.6 | 0.61 | 0.01 | 0.01 | (1) |
| fase 3b call2 #1 | call | 20.1 | 107.5 | 0.68 | 0.01 | 0.01 | 0 |
| fase 3b call2 #2 | call | 19.3 | 106.2 | 0.84 | 0.01 | 0.01 | 0 |
| fase 3b call2 #3 | call | 19.6 | 105 | 0.69 | 0.01 | 0.01 | 0 |
| fase 3b call2 #4 | call | 19.7 | 106.3 | 0.61 | 0.01 | 0.01 | 0 |
| fase 3b call2 #5 | call | 19.8 | 107 | 0.46 | 0.01 | 0.01 | 0 |

"(1)": a linha falsa da primeira versão da vigia (a janela calma depois do tique do autoteste,
corrigida em 3b82ba8). As de (c) e (d) também são da primeira versão (que contava rajadas), e nesses
papéis o autoteste ainda pede um quadro a cada 100 ms (ver "Observações" da fase 1), somado aos
eventos do vídeo; os logs delas não foram guardados, então ficam para a medição final da fase 4.

## Fase 4: medição final (revisão, 2026-10-17)

`tools\perf.ps1 -Label v1.0 -Repeat 3`: **três rodadas de cada cenário** com o build final da branch
v1-ui (fases 1 a 4). A v0.9.1 não foi medida de novo: ela não tem o modo de teste em segundo plano
(as janelas dela aparecem na tela da pessoa e tomam o foco), então a referência é a medição da
fase 1 acima (mesmo script, mesmos cenários). A tabela é a média das rodadas; a faixa de CPU entre
parênteses é a menor e a maior rodada de cada versão.

Critério do veredito, contra o orçamento do plano:

- RAM: no máximo +10% (working set e bytes privados).
- Processador "igual": a média da v1.0 dentro da faixa entre rodadas da v0.9.1 (ou abaixo dela).
- Placa de vídeo "igual": a média da v1.0 até 0,2 ponto acima da v0.9.1. Essa é a variação entre
  rodadas de um mesmo build nas fases 3a (1,53 e 1,71 assistindo) e 3b (1,75 e 1,56); a v0.9.1 só
  tem duas rodadas medidas.

| Cenário | Working set (MB) v0.9.1 → v1.0 | Bytes privados (MB) v0.9.1 → v1.0 | CPU (% de 1 núcleo) v0.9.1 → v1.0 | GPU todos (%) | GPU 3D (%) | Veredito |
|---|---|---|---|---|---|---|
| (a) Tela inicial parada | 81.6 → 82.2 (+0.7%) | 80.2 → 79.7 (-0.6%) | 0.11 → 0.05 (v0.9.1 0.07 a 0.15; v1.0 0.00 a 0.08) | 0.00 → 0.00 | 0.00 → 0.00 | **dentro do orçamento** |
| (b) Chamada com chat | 20.3 → 19.7 (-2.8%) | 108.2 → 108.6 (+0.4%) | 1.61 → 1.15 (v0.9.1 1.53 a 1.69; v1.0 0.92 a 1.30) | 0.00 → 0.01 | 0.00 → 0.01 | **dentro do orçamento** |
| (c) Assistindo transmissão | 59.8 → 60.7 (+1.6%) | 170.5 → 170.0 (-0.3%) | 12.14 → 11.40 (v0.9.1 11.73 a 12.56; v1.0 7.82 a 15.90) | 1.58 → 1.74 | 1.56 → 1.72 | **dentro do orçamento** |
| (d) Câmera ligada | 43.5 → 43.7 (+0.4%) | 227.4 → 227.3 (-0.1%) | 7.75 → 8.07 (v0.9.1 7.29 a 8.21; v1.0 7.28 a 8.52) | 4.31 → 4.47 | 1.98 → 2.10 | **dentro do orçamento** |


**Dentro do orçamento nos quatro cenários.** RAM: no máximo +1,6% (working set assistindo, ~0,9 MB);
bytes privados iguais ou menores. Processador: 0 em repouso na tela inicial; a chamada parada gasta
menos que a v0.9.1 (1,15% contra 1,61%); assistindo, 11,40% contra 12,14%; câmera dentro da faixa
da v0.9.1. Placa de vídeo: igual na tela inicial e na chamada; +0,16 ponto assistindo e com a câmera,
dentro da variação de um mesmo build (ver o critério acima).

### A CPU ao assistir (o 13,6% da fase 3a)

As rodadas de (c) variaram muito nesta medição (7,8, 10,5 e 15,9%), mais do que nas fases
anteriores: o PC estava em uso durante a medição. O próprio app mede o processador a cada 30 s
(linha "process stats" do log) e essa medida ficou estável nas três rodadas: 12,8, 10,2 e 12,8% de
um núcleo, dentro da faixa da v0.9.1 (11,7 a 12,6%). A média das três rodadas (11,40%) e a da fase
3b (11,26%) estão abaixo da v0.9.1; o 13,6% da fase 3a foi uma rodada alta, não uma regressão.

### A vigia de repintura ao assistir e com a câmera

As linhas "continuous repaint" de (c) e (d) na fase 3b vinham do próprio autoteste: elas nomeavam
`autotest.rs` (o tique de 100 ms dos papéis de teste) e `profile.rs:674` (um `request_repaint_after`
que só agenda). O vídeo não pede quadros ao egui: as transmissões e a câmera se apresentam nas
próprias janelas nativas (o log mostra 30,5 quadros apresentados por segundo pelo visualizador
D3D11 enquanto a interface desenha ~12,5 quadros por segundo, dos quais 10 são o tique do teste e
~2,5 são avisos do backend). A vigia agora desconta o tique que o papel de teste declara
(`repaint_watch::set_expected_fps`) e nunca o aponta como causa; um laço de verdade por cima dele
continua sendo pego (teste `a_declared_tick_is_not_a_loop`). Resultado: "busy" 0 em todas as
rodadas, inclusive (c) e (d).

O mesmo tique explica o +0,16 ponto de placa de vídeo em (c) e (d): são ~13 quadros de interface
por segundo nos dois builds, e um quadro da v1.0 desenha um pouco mais (cards arredondados, o dock,
anéis). Para quem usa o app de verdade, sem o tique, a interface desenha ~2,5 quadros por segundo
nesses cenários.

| Rodada | Cenário | WS (MB) | Privados (MB) | CPU (%) | GPU todos (%) | GPU 3D (%) | busy |
|---|---|---|---|---|---|---|---|
| v1.0 #1 | home | 82.3 | 80.6 | 0.08 | 0 | 0 | 0 |
| v1.0 #1 | call | 19.8 | 109.9 | 1.23 | 0.01 | 0.01 | 0 |
| v1.0 #1 | watch | 60.6 | 167.2 | 15.9 | 1.8 | 1.77 | 0 |
| v1.0 #1 | camera | 43.5 | 228.3 | 8.52 | 4.49 | 2.12 | 0 |
| v1.0 #2 | home | 82.2 | 80 | 0.08 | 0 | 0 | 0 |
| v1.0 #2 | call | 19.8 | 105.4 | 1.3 | 0 | 0 | 0 |
| v1.0 #2 | watch | 61 | 170.9 | 7.82 | 1.76 | 1.73 | 0 |
| v1.0 #2 | camera | 43.8 | 228.9 | 8.42 | 4.5 | 2.13 | 0 |
| v1.0 #3 | home | 82 | 78.5 | 0 | 0 | 0 | 0 |
| v1.0 #3 | call | 19.6 | 110.6 | 0.92 | 0.01 | 0.01 | 0 |
| v1.0 #3 | watch | 60.6 | 171.8 | 10.49 | 1.67 | 1.65 | 0 |
| v1.0 #3 | camera | 43.7 | 224.6 | 7.28 | 4.42 | 2.06 | 0 |

Repetir (só o build novo; a v0.9.1 fica com os números da fase 1, ou numa área de trabalho
privada como o espectador antigo do gate):

```
powershell -ExecutionPolicy Bypass -File tools\perf.ps1 -Label v1.0 -Out v1.csv -Repeat 3
```
