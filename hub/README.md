# concord-server

Servidor do concord: login, canais de texto, chat, presença e emissão de tokens do LiveKit. Ele também inicia e vigia o `livekit-server` (voz e Go Live), então o host só precisa abrir um programa.

> Até a v1.0.0 o app se chamava **voxhub** e este programa, `voxhub-server.exe`. Desde a v1.0.1 ele é o `concord-server.exe`: o mesmo servidor, com a mesma pasta de dados (`data\`, o banco continua `voxhub.db`), os mesmos convites e o mesmo protocolo. Apps voxhub v1.0.0 e concord se conectam um ao outro normalmente. O `iniciar-servidor.bat` usa o `bin\concord-server.exe` e, se ele não existir, o antigo `bin\voxhub-server.exe`.

## Rodando (no PC do host)

```powershell
# na raiz do projeto
cargo build --release -p voxhub-server
.\target\release\concord-server.exe --data-dir .\data
```

Opções: `--data-dir <dir>` (padrão `./data`), `--livekit-bin <exe>` (padrão `./bin/livekit-server.exe`, relativo à pasta atual), `--console` (loga também no terminal), `--log-dir <dir>`, `--no-livekit`, `--reset-password <nome>` e `--reset-owner` (os dois últimos estão em [Dono do servidor](#dono-do-servidor-quem-hospeda-vira-dono)). Ctrl+C encerra o servidor e o LiveKit. Se o servidor travar, o Windows encerra o LiveKit junto (Job Object).

Para baixar o LiveKit (versão fixa; o SHA256 do zip oficial está fixado no próprio script e o `checksums.txt` do release é só uma segunda checagem):

```powershell
powershell -ExecutionPolicy Bypass -File hub\scripts\fetch-livekit.ps1
```

## Primeira execução

Cria `<data-dir>\config.toml` com padrões. Edite e reinicie:

- `server_password`: senha compartilhada que os colegas digitam (vazio = sem senha).
- `public_host`: seu IP público ou DDNS. É o endereço que os clientes usam para falar com o LiveKit. Com `127.0.0.1` só o seu PC consegue entrar na voz.
- `[livekit] use_external_ip = true`: faça isso para colegas fora da sua rede, para o LiveKit anunciar o IP público.
- `api_key`/`api_secret` são gerados sozinhos. Não compartilhe.

- `tls`: conexão protegida (v1.1), `"auto"` por padrão. Veja [Conexão protegida](#conexão-protegida-tls-v11).

- `owner`: normalmente **não precisa**. Veja a seção abaixo: quem hospeda vira o Dono sozinho. Escrever `owner = "seu-nome"` (no começo do arquivo, antes de `[livekit]`) é a forma manual, que fixa o Dono pelo nome.

Também ficam em `<data-dir>`: `voxhub.db` (SQLite, modo WAL), `livekit.yaml` (gerado a cada início) e, desde a v1.1, a pasta `tls\` com o certificado do servidor.

## Conexão protegida (TLS, v1.1)

Desde a v1.1 o servidor fala **TLS na mesma porta** (7890): o app 1.1 (ou mais novo) conecta com TLS e um app antigo continua conectando sem, na mesma porta (o servidor olha o primeiro byte de cada conexão). Senhas, convites, logins lembrados, mensagens e imagens vão criptografados.

- **Certificado próprio.** Na primeira vez que liga, o servidor cria um certificado autoassinado (ECDSA P-256, válido por 100 anos) em `<data-dir>\tls\server.key` (a chave, secreta) e `server.crt`. Não há autoridade certificadora: o app guarda o SHA-256 desse certificado (o *pin*) e só conversa com ele. Os convites novos já levam o pin; quem digita o endereço aceita o certificado na primeira vez. A janela do servidor mostra o **código do servidor** (os 10 primeiros bytes do pin em base32, como `K7QM-2D9X-4HVA-PB1C`), o mesmo que o app mostra em "Segurança da conexão", para comparar.
- **Ao mudar o servidor de PC, leve a pasta `tls\` junto com o resto de `data\`.** Um certificado novo faz todos os apps dizerem "A identidade do servidor mudou" (e eles não mandam nada até a pessoa usar um convite novo ou confiar no código novo).
- **A voz também passa pela porta do servidor.** Pela conexão protegida, a sinalização do LiveKit (`/rtc`) entra pela porta 7890 e o servidor a repassa ao LiveKit no próprio PC (`127.0.0.1:7880`). A mídia continua nas portas 7881/7882. Apps antigos ainda usam a 7880 direto.
- **`tls` no `config.toml`:**
  - `"auto"` (padrão): apps 1.1 com proteção, apps antigos sem.
  - `"required"`: só conexões protegidas. Um app antigo recebe `426` com a mensagem "Este servidor exige o concord 1.1 ou mais novo (conexão protegida)." O próprio PC do servidor (`127.0.0.1`) pode sempre entrar sem. No app, "Meu servidor" > "Aceitar apps antigos sem conexão protegida" faz o mesmo para servidores criados pelo app (ligado = `auto`, desligado = `required`, vale na hora).
  - `"off"`: sem TLS nenhum, como antes da v1.1 (não recomendado).
- **Não volte para um servidor antigo (downgrade).** Um app que já usou a conexão protegida com um servidor **nunca mais** conecta nele sem ela: se o servidor voltar para a v1.0.x (ou para `tls = "off"`), esses apps dizem que não conseguem conectar com proteção. É de propósito (um atacante não consegue "rebaixar" a conexão). Para voltar mesmo assim, cada pessoa tira o servidor da lista no app ("Remover da lista", ou seja, esquecer o servidor) e adiciona de novo.

## Dono do servidor (quem hospeda vira dono)

O **Dono** é quem pode tudo no servidor (dar cargo de Admin, redefinir a senha pessoal de alguém, etc.). Quem hospeda o servidor é o Dono, sem editar arquivo nenhum:

- **Entrando pelo próprio PC do servidor.** Enquanto o servidor **não tem dono**, o primeiro login (ou o primeiro app reconectando, se já estava aberto) cuja conexão vem do **próprio PC do servidor** vira o Dono na hora. O servidor decide pelo endereço da conexão TCP (`127.0.0.1`/`::1`), nunca por cabeçalho, então **no app do PC do host use o endereço `http://127.0.0.1:7890`** (como na instalação). Entrar pelo IP da rede local ou pelo IP público, mesmo estando no PC do servidor, **não conta**. Em seguida o app pede a **senha pessoal** do Dono (os poderes só ligam depois dela) e a posse fica no registro de moderação ("virou o Dono do servidor").
- **Código de reivindicação (plano B).** Ao ligar sem dono, a janela do servidor mostra um código de uso único e também o grava no log:

  ```
  Nenhum dono ainda. Para virar dono, entre no app e use o código: K7QM-2XD9
  (ou entre pelo próprio PC do servidor: use o endereço 127.0.0.1)
  ```

  No app, quem estiver logado usa **"Reivindicar servidor"** (botão "Sem dono" ao lado do nome do servidor, ou em Configurações) e digita o código. Ele vale até alguém reivindicar, **muda a cada vez que o servidor liga** sem dono, é comparado em tempo constante e erros seguidos fazem o servidor inteiro esperar (1 s, 2 s, 4 s... até 1 minuto; vale para qualquer endereço). Depois de usado não funciona mais.
- **Manual.** Com `owner = "nome"` no `config.toml`, o Dono é esse nome, nada é reivindicado e a posse não pode ser transferida pelo app.

Servidor que já existia (v0.4.1/v0.5.0 sem `owner`): ao atualizar, o banco ganha só a tabela `server_meta` (migração aditiva e repetível; as versões anteriores continuam rodando nele). Abra o app **no PC do servidor, com `127.0.0.1`**: você vira o Dono sozinho (inclusive se o app já estava aberto e só reconectou).

**Segurança.** Um proxy reverso ou túnel rodando no mesmo PC (ngrok, cloudflared, nginx...) faz todo mundo parecer vir de `127.0.0.1`. Por isso o servidor **não** reivindica automaticamente quando a requisição traz `Forwarded`, `X-Forwarded-*`, `X-Real-IP` ou um `Host` que não é `localhost`/`127.0.0.1`/`::1` (cabeçalhos só podem **negar**, nunca conceder). Um túnel que apaga esses cabeçalhos ainda enganaria a regra: se você usa túnel, ponha `owner = "seu-nome"` no `config.toml`, ou entre antes que qualquer outra pessoa consiga alcançar o servidor.

**Transferir a posse.** O Dono abre "🛠 Servidor" > Membros e usa **"Transferir posse do servidor"** na linha da pessoa (com confirmação). A pessoa precisa ter **senha pessoal**; o Dono antigo vira Admin e a troca entra no registro de moderação. Com `owner` fixo no `config.toml` o botão fica desligado (a explicação aparece ao passar o mouse): troque o nome no arquivo e reinicie.

**Recuperação.** Os dois comandos mexem só no banco, então podem rodar com o servidor ligado (ele percebe em até 2 s) ou desligado, e não iniciam o servidor:

```powershell
# esqueceu a senha pessoal de alguém (inclusive do Dono): mostra um código de uso único (vale até 3 dias)
.\concord-server.exe --data-dir .\data --reset-password <nome>

# o Dono sumiu ou perdeu o acesso: o servidor fica sem dono
.\concord-server.exe --data-dir .\data --reset-owner
```

Depois de `--reset-owner` o servidor mostra um código novo na janela dele e quem entrar pelo PC do servidor vira o novo Dono (se o app do Dono antigo continuar aberto no PC do servidor, ele reconecta e reassume sozinho: feche-o antes se for passar para outra pessoa). `--reset-owner` não mexe no `owner` do `config.toml`: se o dono estiver lá, tire a linha. Também fica no registro ("Host (linha de comando)").

## Canais privados (v0.9.0)

Em "🛠 Servidor" > Canais, Admin e Dono marcam um canal (de texto ou de voz) como **Canal privado** e escolhem quem entra: os cargos Moderador e/ou Membro, e/ou pessoas específicas. **Dono e Admin sempre têm acesso.** Quem não tem acesso não vê nada: o canal não aparece, não gera aviso nem contador, o histórico, as imagens e as reações dele respondem "não encontrado", e a voz recusa a entrada. Quem perde o acesso (cargo trocado, tirado da lista, canal que virou privado) deixa de ver o canal na hora e, se estava na voz dele, é desconectado. Apps antigos (v0.8.x) simplesmente não veem os canais privados a que a pessoa não tem acesso. Cada mudança entra no registro de moderação.

> **ATENÇÃO: não volte para um servidor antigo.** Um `voxhub-server` v0.8.x (ou mais antigo) não conhece canais privados e, rodando neste banco de dados, mostraria esses canais para todo mundo. Para isso não acontecer, enquanto existir algum canal privado o banco fica marcado de um jeito que **o servidor antigo se recusa a iniciar** (no console aparece um erro como `Invalid column type Text ... position`). Se precisar mesmo voltar para a versão antiga, abra a v0.9.0 antes e torne todos os canais públicos (ou apague os privados): aí o servidor antigo volta a funcionar com o mesmo banco.

## Portas no roteador

Redirecione para o PC do host:

| Porta | Protocolo | Uso |
|---|---|---|
| 7890 | TCP | API e chat (hub) |
| 7880 | TCP | sinalização do LiveKit (apps antigos; o app 1.1 usa a 7890) |
| 7881 | TCP | mídia do LiveKit (fallback quando UDP é bloqueado) |
| 7882 | UDP | mídia do LiveKit (principal) |

No servidor separado (zip `concord-servidor`, v1.0.2 em diante), rode o `liberar-firewall.bat` antes de ligar: ele cria regras de entrada no Firewall do Windows só para `bin\concord-server.exe` e `bin\livekit-server.exe` e só nas portas acima, em todas as redes (Domínio, Privada e Pública), em IPv4 e IPv6, e desliga bloqueios que o próprio Windows tenha criado para esses programas (o `remover-firewall.bat` desfaz tudo). Se o Firewall perguntar ao ligar o servidor, permita nas redes que você usa: se o Windows marcou a rede de casa como Pública, marque também redes públicas, senão ninguém de fora entra. Num notebook que se conecta a Wi-Fi público, desligue o servidor fora de casa. Uma VPN (Tailscale ou ZeroTier) é mais segura do que abrir portas; veja `SECURITY.md`.

## IPv6 (v1.0.2)

O servidor escuta em IPv4 e IPv6 num socket só (`bind = "[::]:7890"`; um `config.toml` antigo com `0.0.0.0:7890` também passa a escutar nos dois). Amigos com IPv6 entram pelo IPv6 global do PC sem redirecionar portas, mesmo com CGNAT, e os convites criados no servidor já levam esse endereço quando `public_host` está vazio ou é `127.0.0.1` (o padrão). Alguns roteadores bloqueiam IPv6 vindo de fora: procure "firewall IPv6" nas configurações dele. Para voltar a só IPv4, acrescente `ipv6 = false` no `config.toml`. Alguns segundos depois de ligar, o servidor mostra um diagnóstico de conexão (IPv4/IPv6, CGNAT, Firewall do Windows); `--no-diag` desliga.

## CGNAT

Se a operadora usa CGNAT (o IP "público" do roteador não é o IP que sites como o ifconfig.me mostram, ou fica em 100.64.x.x), o redirecionamento de portas não funciona. Alternativas: IPv6 (veja acima), VPN mesh (Tailscale ou ZeroTier; use o IP da VPN em `public_host`) ou uma VPS pequena como relay.

## Logs

`%LOCALAPPDATA%\concord\logs\hub\hub.<data>.log`, rotação diária, 5 arquivos. Nível `info` por padrão; para investigar: `$env:VOX_LOG="debug"`. Segredos e tokens nunca são logados. As linhas de aviso e erro do LiveKit aparecem com o alvo `livekit`.

## Testes

```powershell
cargo test -p voxhub-server
```

Sobem o servidor em porta efêmera, sem LiveKit, com diretório temporário.
