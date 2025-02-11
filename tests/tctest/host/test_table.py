import random
import tinychain as tc
import unittest

from num2words import num2words
from .base import HostTest

ENDPOINT = "/transact/hypothetical"
SCHEMA = tc.table.Schema(
    [tc.Column("name", tc.String, 512)], [tc.Column("views", tc.UInt)]).create_index("views", ["views"])


class TableTests(HostTest):
    def testCreate(self):
        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, expected(SCHEMA, []))

    def testDelete(self):
        count = 10
        values = [(v,) for v in range(count)]
        keys = [(num2words(i),) for i in range(count)]

        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.inserts = [cxt.table.insert(k, v) for k, v in zip(keys, values)]
        cxt.delete = tc.after(cxt.inserts, cxt.table.delete(("one",)))
        cxt.result = tc.after(cxt.delete, cxt.table.count())

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, count - 1)

    def testInsert(self):
        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.upsert = cxt.table.upsert((num2words(1),), (1,))
        cxt.result = tc.after(cxt.upsert, cxt.table.count())

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, 1)

        for x in range(0, 20, 5):
            keys = list(range(x))
            random.shuffle(keys)

            cxt = tc.Context()
            cxt.table = tc.table.Table(SCHEMA)
            cxt.inserts = [
                cxt.table.insert((num2words(i),), (i,))
                for i in keys]

            cxt.result = tc.after(cxt.inserts, cxt.table.count())

            result = self.host.post(ENDPOINT, cxt)
            self.assertEqual(result, x)

    def testLimit(self):
        count = 50
        values = [(v,) for v in range(count)]
        keys = [(num2words(i),) for i in range(count)]

        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.inserts = [cxt.table.insert(k, v) for k, v in zip(keys, values)]
        cxt.result = tc.after(cxt.inserts, cxt.table.limit(1))

        result = self.host.post(ENDPOINT, cxt)
        first_row = sorted(list(k + v) for k, v in zip(keys, values))[0]
        self.assertEqual(result, expected(SCHEMA, [first_row]))

    def testSelect(self):
        count = 5
        values = [[v] for v in range(count)]
        keys = [[num2words(i)] for i in range(count)]

        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.inserts = [cxt.table.insert(k, v) for k, v in zip(keys, values)]
        cxt.result = tc.after(cxt.inserts, cxt.table.select(["name"]))

        expected = {
            str(tc.URI(tc.table.Table)): [
                tc.to_json(tc.table.Schema([], [tc.Column("name", tc.String, 512)])),
                list(sorted(keys))
            ]
        }

        actual = self.host.post(ENDPOINT, cxt)

        self.assertEqual(actual, expected)
    @unittest.skip
    def testAggregate(self):
        count = 10
        values = [(v % 2,) for v in range(count)]
        keys = [(num2words(i),) for i in range(count)]

        cxt = tc.Context()
        cxt.table = tc.table.Table.load(SCHEMA, [k + v for k, v in zip(keys, values)])
        cxt.result = cxt.table.aggregate(["views"], lambda group: tc.Tuple(group.count()))

        actual = self.host.post(ENDPOINT, cxt)
        self.assertEqual(actual, [[[0], 5], [[1], 5]])

    def testTruncateSlice(self):
        count = 50
        values = [[v] for v in range(count)]
        keys = [[num2words(i)] for i in range(count)]
        remaining = sorted([k + v for k, v in zip(keys, values) if v[0] >= 40])

        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.inserts = [cxt.table.insert(k, v) for k, v in zip(keys, values)]
        cxt.delete = tc.after(cxt.inserts, cxt.table.where(views=slice(40)).truncate())
        cxt.result = tc.after(cxt.delete, cxt.table)

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, expected(SCHEMA, remaining))

    def testUpdateSlice(self):
        count = 50
        values = [[v] for v in range(count)]
        keys = [[num2words(i)] for i in range(count)]

        cxt = tc.Context()
        cxt.table = tc.table.Table.load(SCHEMA, [k + v for k, v in zip(keys, values)])
        cxt.update = cxt.table.where(views=slice(10)).update(views=0)
        cxt.result = tc.after(cxt.update, cxt.table.where(views=slice(1)).count())

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, 10)

    def testOrderBy(self):
        count = 50
        values = [(v,) for v in range(count)]
        keys = [(num2words(i),) for i in range(count)]
        rows = list(reversed([list(k + v) for k, v in zip(keys, values)]))

        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.inserts = [cxt.table.insert(k, v) for k, v in zip(keys, values)]
        cxt.result = tc.after(cxt.inserts, cxt.table.order_by(["views"], True))

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, expected(SCHEMA, rows))

    def testSlice(self):
        count = 50
        values = [(v,) for v in range(count)]
        keys = [(num2words(i),) for i in range(count)]

        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.inserts = [cxt.table.insert(k, v) for k, v in zip(keys, values)]
        cxt.result = tc.after(cxt.inserts, cxt.table.where(name="one"))

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, expected(SCHEMA, [["one", 1]]))

    def testSliceAuxiliaryIndex(self):
        count = 50
        values = [(v,) for v in range(count)]
        keys = [(num2words(i),) for i in range(count)]

        cxt = tc.Context()
        cxt.table = tc.table.Table(SCHEMA)
        cxt.inserts = [cxt.table.insert(k, v) for k, v in zip(keys, values)]
        cxt.result = tc.after(cxt.inserts, cxt.table.where(views=slice(10, 20)))

        result = self.host.post(ENDPOINT, cxt)
        self.assertEqual(result, expected(SCHEMA, list([[num2words(i), i] for i in range(10, 20)])))


def expected(schema, rows):
    return {str(tc.URI(tc.table.Table)): [tc.to_json(schema), rows]}


if __name__ == "__main__":
    unittest.main()
