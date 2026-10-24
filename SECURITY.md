# Segurança (concord v1.1)

## Para quem o concord foi feito

Para **grupos pequenos de amigos que confiam entre si**, com o servidor rodando no PC de um deles.
Ele não foi feito para comunidades abertas com desconhecidos.

## A conexão é protegida (TLS) desde o concord 1.1

- Entre o app 1.1 (ou mais novo) e um servidor 1.1 (ou mais novo), **tudo vai criptografado**
  (TLS): senhas, códigos de convite, logins lembrados, mensagens, imagens e a conexão da voz. O
  áudio e o vídeo das chamadas já iam criptografados antes.
- **Certificado do próprio servidor.** O servidor cria um certificado só dele na primeira vez que
  liga (fica na pasta `tls`, ao lado do `config.toml`). Não existe autoridade certificadora: o
  app guarda a "impressão digital" desse certificado e, dali em diante, só conversa com ele.
  - **Com um convite novo**, o convite já traz essa impressão digital: o app conecta direto no
    certificado certo, nunca sem proteção.
  - **Digitando o endereço**, o app aceita o certificado na primeira vez e o guarda para as
    próximas (como o SSH). Para conferir, compare o **código do servidor** (menu do servidor →
    "Segurança da conexão", por exemplo `K7QM-2D9X-4HVA-PB1C`) com o que aparece na janela do
    servidor ou no painel "Meu servidor" de quem hospeda.
  - **Se o certificado mudar**, o app avisa "A identidade do servidor mudou" e **não manda nada**
    (nem senha, nem login lembrado). Se quem hospeda reinstalou o servidor, peça um convite
    novo. Se não, alguém pode estar interceptando a conexão.
- **O cadeado** no topo da lista de canais mostra o estado: fechado ("Conexão protegida") ou
  aberto ("Conexão sem proteção (servidor antigo)").
- **Servidor antigo (antes do 1.1):** não tem TLS. O app ainda conecta, sem proteção e com o
  cadeado aberto (num endereço digitado, ele pergunta antes). Nesse caso senhas e mensagens
  passam pela rede sem criptografia, como antes: atualize o servidor.
- **Servidores salvos no app 1.0:** no primeiro login pelo app 1.1, o app usa a conexão
  protegida se o servidor tiver e passa a exigir aquele certificado. Até esse primeiro login
  protegido, alguém que controle a rede ainda pode fazer o servidor parecer antigo (sem
  proteção, sem perguntar): confira se o cadeado fica fechado e, na dúvida, entre com um convite
  novo (ele já traz o certificado).
- **App antigo (antes do 1.1) num servidor novo:** por padrão ainda entra, sem proteção. Quem
  hospeda pode recusar apps antigos: no `config.toml`, `tls = "required"` (ou, no app, desligar
  "Aceitar apps antigos sem conexão protegida" em "Meu servidor"); eles recebem um aviso para
  atualizar. O próprio PC do servidor (endereço 127.0.0.1) pode sempre entrar.
- **Depois de usar conexão protegida, o app nunca mais volta a conectar sem ela** naquele
  servidor. Se quem hospeda voltar para uma versão antiga do servidor, os apps vão dizer que não
  conseguem conectar com proteção: tire o servidor da lista ("Remover da lista", ou seja,
  esquecer o servidor) e adicione de novo.
- **Guarde a pasta `tls` ao mudar o servidor de PC** (ela vai junto com a pasta `data\`). Sem
  ela o servidor cria um certificado novo, e todo mundo vê o aviso de identidade mudada.
- O TLS protege o caminho entre o app e o servidor. **Quem hospeda continua vendo tudo** o que
  passa pelo servidor (o banco de dados guarda as mensagens).
- **Use uma senha de servidor forte e exclusiva**, e uma senha pessoal que você não usa em
  nenhum outro lugar.
- **Uma VPN (Tailscale ou ZeroTier) continua sendo mais segura do que abrir portas no
  roteador**: só quem está na sua VPN alcança o servidor.
- **O app pode abrir as portas do roteador sozinho.** Quando você usa "Criar servidor" no app, ele
  pede ao roteador (UPnP) que abra as portas do servidor para a internet, e as fecha ao parar. É
  o que permite amigos de fora entrarem sem configurar nada, mas também deixa o servidor visível
  na internet enquanto ele está ligado. Se preferir não expor nada, desligue o UPnP no roteador e
  use uma VPN.
- Se você mesmo abrir portas, abra **só** as do concord (TCP 7890, 7880 e 7881, UDP 7882). Evite
  o DMZ do roteador, que expõe o PC inteiro. Mantenha o Firewall do Windows ligado. No servidor
  separado, o `liberar-firewall.bat` libera só os programas e as portas do concord, em todas as
  redes (também nas que o Windows marca como Públicas, senão quem é de fora não entra). Num
  notebook que usa Wi-Fi público, desligue o servidor fora de casa ou rode o `remover-firewall.bat`.

## Convites

- **Um código de convite (`vox1-...`) funciona como uma senha**: quem tem o código entra no
  servidor sem a senha do servidor. Mande-o só em conversa privada, nunca num lugar público.
- Dá para pôr validade e limite de usos, e **cancelar** um convite a qualquer momento (o Dono ou
  um Admin, em "Convidar amigos"). Quem já entrou com ele continua no servidor.
- Um convite só **cria contas novas**. Ele não serve para entrar com o nome de alguém que já tem
  conta no servidor: esse nome continua precisando da senha do servidor (ou da própria senha
  pessoal da pessoa).
- O app só manda a senha do servidor que ele guardou, e o login lembrado deste computador, para o
  **mesmo servidor** em que eles foram salvos (cada servidor tem uma identidade própria). Se um
  endereço, de um convite ou da lista, responder como outro servidor, o app avisa ("Este endereço
  não é o mesmo servidor que você salvou") e não manda nada do que está salvo.

## Onde ficam os segredos

- **Servidor criado pelo app ("Criar servidor"):** cada servidor fica em
  `%LOCALAPPDATA%\concord\host\<id>\`. Ali estão o `config.toml` (com a senha do servidor e a
  chave do servidor de voz) e o banco de dados, com as mensagens.
- **Servidor rodando pelo `iniciar-servidor.bat`:** tudo isso fica na pasta `data\`.
- **O certificado do servidor** (v1.1) fica em `tls\` dentro dessa pasta: `server.key` é a chave
  secreta (quem a tiver pode se passar pelo servidor), `server.crt` é público.
- **A pasta `data\` (e a de cada servidor em `host\`) é secreta.** Nunca envie essas pastas para
  ninguém nem para o GitHub, e não as deixe numa pasta compartilhada.
- **No app de cada pessoa:** a senha do servidor (só com "Lembrar a senha"), o login lembrado
  deste computador e a chave de criptografia deste computador (veja abaixo) ficam:
  - **no Windows**, criptografados para o seu usuário do Windows (DPAPI), em `%APPDATA%\concord\`;
  - **no Mac** (desde a 1.1.0), no arquivo `secrets.json` em
    `~/Library/Application Support/concord/`, que só o seu usuário do Mac consegue ler (permissão
    0600). Eles **não ficam mais nas Chaves (Keychain)**: como o app não é assinado por um
    desenvolvedor registrado, o Mac pedia a senha das Chaves depois de cada atualização. Quem
    usar o seu usuário do Mac (ou um programa rodando nele) consegue ler esse arquivo, como
    acontece com a maioria dos apps de desktop. Na primeira vez, a 1.1.0 copia das Chaves o que
    as versões anteriores guardaram ali (o Mac pode perguntar uma última vez); se você negar,
    basta digitar a senha do servidor de novo.
- **Quem hospeda guarda os dados.** Mensagens, inclusive as diretas, e imagens ficam no banco de
  dados do PC de quem hospeda. O app não deixa ninguém (nem Admin, nem Dono) ler as mensagens
  diretas dos outros, mas quem tem acesso ao arquivo do banco consegue.

## Criptografia de ponta a ponta (v1.1)

- **Cada computador tem a sua chave** (Configurações → Privacidade → Seus aparelhos). O servidor
  guarda só a parte pública e avisa quando aparece um aparelho novo na conta de alguém.
- **Mensagens diretas** (texto e imagens) entre apps 1.1 vão criptografadas de ponta a ponta: o
  banco de dados de quem hospeda guarda só o conteúdo cifrado. Confira o **código de segurança**
  da conversa com a outra pessoa por outro meio; se ele mudar, o app avisa.
- **Chamadas diretas** (voz, câmera e transmissão) são criptografadas sozinhas quando os dois lados
  usam a 1.1. **Salas de voz** só quando um Admin liga a opção na sala (com ela vazia); apps 1.0 não
  entram nessas salas.
- **Não protege:** os metadados (quem fala com quem, quando, tamanhos, reações) e os canais de
  texto (a criptografia dos canais vem na 1.2). Um servidor modificado de propósito pode tentar
  enfiar um aparelho falso: o app avisa ("novo aparelho", código mudou), mas não impede.

## Canais privados

- Quem não tem acesso a um canal privado **não vê o canal, as mensagens, as imagens, as reações
  nem quem está na voz dele**, e não consegue entrar na voz.
- Mas dá para **perceber que existe atividade escondida**: as mensagens do servidor são
  numeradas em sequência, e um app modificado nota os números que pulam (quantas mensagens foram
  mandadas em canais que ele não vê, e quando). O conteúdo, os nomes e os canais não vazam.
- Quem perde o acesso sai da voz do canal na hora. A entrada da voz vale só por cerca de um
  minuto, então quem perdeu o acesso não consegue voltar.

## Não volte para um servidor antigo (proteção contra downgrade)

Um servidor antigo (v0.8.x ou anterior) não conhece canais privados e, rodando no mesmo banco de
dados, mostraria esses canais para todo mundo. Por isso, enquanto existir algum canal privado, o
banco fica marcado de um jeito que **o servidor antigo se recusa a iniciar**. Se precisar mesmo
voltar, abra a versão nova antes e torne todos os canais públicos (ou apague os privados). Os
detalhes estão em `hub/README.md`.

## Outras dicas

- **Defina a senha do servidor.** Sem ela (`server_password` vazio no `config.toml`), qualquer
  pessoa que descobrir o endereço pode entrar.
- **Proteja o seu nome** com uma senha pessoal (Configurações → Senha pessoal). Sem ela, quem
  tem a senha do servidor pode usar o seu nome. Dono, Admin e Moderador são obrigados a ter uma.
- **Baixe o app só de fontes que você confia** (o repositório oficial ou a pessoa que hospeda o
  seu grupo). O app não é assinado digitalmente, então o Windows (SmartScreen) e o Mac
  (Gatekeeper) mostram um aviso na primeira vez; os LEIA-ME explicam como abrir.

## Encontrou uma falha?

Não publique detalhes que permitam explorar a falha. Use a opção "Report a vulnerability"
(Security Advisories) do GitHub, se estiver habilitada, ou abra uma *issue* sem os detalhes
pedindo um contato privado. Diga a versão do concord e, se puder, como reproduzir com contas de
teste.
