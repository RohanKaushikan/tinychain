from tinychain.chain import Sync
from tinychain.cluster import Cluster
from tinychain.collection.schema import Column, Graph as Schema, Table as TableSchema
from tinychain.collection.table import Table
from tinychain.collection.tensor import Sparse
from tinychain.ref import After, Before, If
from tinychain.state import Map, Tuple
from tinychain.value import Bool, I64, U64


NODE_ID = "node_id"


class Graph(Cluster):
    def _configure(self):
        schema = self._schema()

        nodes = set()
        for from_node, to_node in schema.edges.values():
            nodes.add(from_node)
            nodes.add(to_node)

        for node in nodes:
            if '.' in node:
                table_name, column_name = node.split('.')
            else:
                raise ValueError(f"{node} must specify a column")

            [key] = [col for col in schema.tables[table_name].columns() if col.name == column_name]
            setattr(self, f"node_{table_name}_{column_name}", schema.chain(node_table(key)))

        for name in schema.edges:
            setattr(self, f"edges_{name}", schema.chain(Sparse.zeros([I64.max(), I64.max()], Bool)))

        for name in schema.tables:
            if hasattr(self, name):
                raise ValueError(f"Graph already has an entry called f{name}")

            setattr(self, name, schema.chain(graph_table(self, schema, name)))

    def _schema(self):
        return Schema(Sync)


def graph_table(graph, schema, table_name):
    edges = {}
    prefix = f"{table_name}."
    for from_node, to_node in schema.edges.values():
        if from_node.startswith(prefix):
            col_name = from_node[len(prefix):]
            edges[col_name] = getattr(graph, f"node_{table_name}_{col_name}")

    table_schema = schema.tables[table_name]
    key_columns = [col.name for col in table_schema.key]
    value_columns = [col.name for col in table_schema.values]

    class GraphTable(Table):
        def is_unique(self, col_name, value):
            return self.count({col_name: value}) <= 1

        def delete_row(self, key):
            # for each edge whose source is this table
            #   if this row's value is unique, delete the node from the node table

            row = Map(Before(Table.delete_row(self, key), self[key]))

            deletes = []
            for col_name in edges:
                value = row[col_name]
                delete = If(self.is_unique(col_name, value), edges[col_name].remove(value))
                deletes.append(delete)

            return deletes

        def update_row(self, key, values):
            # for each edge whose source is this table
            #   if the value is being updated
            #       if this row's value is unique, delete the node from the node table
            #       insert the new value into the node table

            row = Map(Before(Table.update_row(self, key, values), self[key]))

            updates = []
            for col_name in edges:
                edge = edges[col_name]
                old_value = row[col_name]
                maybe_remove = If(self.is_unique(col_name, old_value), edge.remove(old_value))

                if col_name in value_columns:
                    update = If(values.contains(col_name), (edge.add(values[col_name]), maybe_remove))
                else:
                    new_value = key[key_columns.index(col_name)]
                    update = If(old_value != new_value, (edge.add(new_value), maybe_remove))

                updates.append(update)

            return updates

        def upsert(self, key, values):
            # for each edge whose source is this table
            #   if the new value is different
            #       if this row's value is unique, delete the node from the node table
            #   insert the new value into the node table

            row = Map(Before(Table.upsert(self, key, values), self[key]))

            updates = []
            for col_name in edges:
                if col_name in key_columns:
                    new_value = key[key_columns.index(col_name)]
                else:
                    new_value = values[value_columns.index(col_name)]

                edge = edges[col_name]
                old_value = row[col_name]
                maybe_remove = If(self.is_unique(col_name, old_value), edge.remove(old_value))
                updates.append(If(old_value != new_value, (edge.add(new_value), maybe_remove)))

            return updates

    return GraphTable(table_schema)


def node_table(key_column):
    class NodeTable(Table):
        def add(self, key):
            return If(self.contains([key]), None, self.upsert(key, [self.create_id(key)]))

        def create_id(self, key):
            new_id = self.max_id()
            return After(self.insert([key, new_id]), new_id)

        def get_id(self, key):
            return self[(key,)][NODE_ID]

        def get_or_create_id(self, key):
            return If(self.contains([key]), self.get_id(key), self.create_id(key))

        def max_id(self):
            row = self.order_by([NODE_ID], True).select([NODE_ID]).rows().first()
            return If(row.is_none(), 0, Tuple(row)[0])

        def remove(self, key):
            return self.delete_row([key])

    schema = TableSchema([key_column], [Column(NODE_ID, U64)])
    return NodeTable(schema.create_index(NODE_ID, [NODE_ID]))
