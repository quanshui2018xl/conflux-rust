import pytest
from integration_tests.test_framework.test_framework import ConfluxTestFramework
from integration_tests.test_framework.util import *

@pytest.fixture(scope="module")
def framework_class():
    class Framework(ConfluxTestFramework):
        def set_test_params(self):
            self.num_nodes = 2
            self.conf_parameters["evm_chain_id"] = str(10)
            self.conf_parameters["evm_transaction_block_ratio"] = str(1)
            self.conf_parameters["executive_trace"] = "true"
            self.conf_parameters["cip1559_transition_height"] = str(1)
            self.conf_parameters["min_eth_base_price"] = 20 * (10**9)
            self.conf_parameters["tx_pool_allow_gas_over_half_block"] = "true"
    return Framework

def test_tx_and_receipt(cw3, ew3, erc20_contract, evm_accounts, network):
    csc_contract = cw3.cfx.contract(name="CrossSpaceCall", with_deployment_info=True)
    new_account = ew3.eth.account.create()
    receipt = csc_contract.functions.transferEVM(new_account.address).transact({
        "value": cw3.to_wei(1, "ether")
    }).executed()
    epoch = receipt["epochNumber"]
    ret = network.nodes[0].debug_getTransactionsByEpoch(hex(epoch))
    assert_equal(len(ret), 1)