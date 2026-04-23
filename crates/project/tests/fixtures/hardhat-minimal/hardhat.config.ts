import { HardhatUserConfig } from "hardhat/config";

const config: HardhatUserConfig = {
    solidity: "0.8.24",
    paths: {
        sources: "./contracts",
        tests: "./test",
    },
};

export default config;
