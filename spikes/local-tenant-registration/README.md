# Local tenant registration spike

Fixture automatizada da RDB-005. Ela cria dois databases temporários reservados no MySQL local:

```text
reprodb_spike_central
reprodb_spike_tenant_data
```

Em seguida, inicia o tenancy real do Salt por um domain local, confirma o override de database e consulta uma tabela do tenant.

```bash
./run.sh
```

O teste recusa sobrescrever databases preexistentes e remove os dois databases ao sair. Por padrão, espera os repositórios `reprodb` e `Salt` lado a lado e o container local `mysql-8`; ambos podem ser alterados por variáveis `REPRODB_SPIKE_*`.

O JSON central da fixture contém somente `tenancy_db_name` e duas configurações não sensíveis. Nenhum JSON de tenant real é lido ou copiado.
